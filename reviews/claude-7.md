<img src="https://r2cdn.perplexity.ai/pplx-full-logo-primary-dark%402x.png" style="height:64px;margin-right:32px"/>

# Подтяни код из мастера и сделай ревью. Мне нужно по смыслу. какие там смысловые пробелы? Почему он не cжимает сейчас? и как это пофиксить?

Теперь картина полная. Вот честное ревью.

***

## Главный диагноз: два несвязанных compressor'а в одном файле

В `codec.rs` живут **две параллельные системы**, которые никогда не были интегрированы друг с другом.

### Система A — `StrictOperatorKey` (production путь)

`try_encode_operator_block` вызывает `compile_strict_operator_key` → получает `StrictOperatorKey { bit_len, bytes }` — это плотная битовая упаковка топологических параметров (phase, affine, shift, rotate, lane, multiplier, parity). Затем вызывает `apply_strict_operator_schedule` → получает `breadcrumbs: Vec<u8>`. Breadcrumbs пишутся в wire format.

`decode_operator_block` **breadcrumbs читает, но не использует для восстановления**. Он восстанавливает данные через `apply_strict_operator_inverse_step` (математическая инверсия), и только потом прогоняет forward pass для верификации breadcrumbs. То есть breadcrumbs — это мёртвый груз в wire format: они занимают место, но декодер их игнорирует как вектор восстановления.

### Система B — `PackedConstantK` / `contract_block` (test-only путь)

`compile_topology_to_constant` → `PackedConstantK` — это совершенно другой ключ: `ConstantFamily`, `RoutingKind`, `branch_rounds`, `phase_mask`, `affine_mask`, `odd_multiplier` и т.д. Всё это под `#[cfg(test)]` в `operator.rs` и нигде не используется в production. `contract_block` принимает `RuntimePipeline` (строится через `materialize_runtime` из `PackedConstantK`), выполняет collapse-passes и возвращает seed + branches. Branches — это то, что должно быть в wire format вместо биективных breadcrumbs.

***

## Почему не сжимает

### 1. Breadcrumbs из биекции — это ~50% случайный шум

`apply_strict_operator_schedule` — биективная перестановка. На каждом шаге она записывает 1 бит на слово (`word_count * steps` бит). Поскольку операция биективна, breadcrumbs не несут структуры данных — они так же несжимаемы, как сам исходник. `operator_budget` это честно считает через `crumb_bits`, и `CompressionPolicy::accepts` проверяет `encoded_bits < source_bits`. Блок принимается только если `key_bits + crumb_bits + overhead_bits < source_bits`. Но `crumb_bits ≈ source_bits * (steps / 64)` для одного шага, плюс overhead. Для большинства блоков это не проходит.

### 2. `BitBudget` переименован, но `codec.rs` использует старые поля

Смотри `budget.rs`: `BitBudget` теперь имеет поля `seed_bits` и `branch_bits` (не `crumb_bits`).  Но `codec.rs` в `operator_budget()` и `try_encode_trajectory_block()` создаёт `BitBudget` со старым полем `crumb_bits`:

```rust
// codec.rs — operator_budget():
Ok(BitBudget {
    source_bits: (source_len_bytes * 8) as u64,
    key_bits: operator.key_bits as u64,
    crumb_bits: ((operator.breadcrumbs.len() + operator.terminal_payload.len()) * 8) as u64,
    overhead_bits: (overhead_bytes * 8) as u64,
})
```

Это **не компилируется**. Поле `crumb_bits` в `BitBudget` больше не существует — есть `seed_bits` и `branch_bits`. Значит либо код не собирается, либо ты видишь старую версию одного из файлов. Скорее всего `budget.rs` уже обновлён под новую семантику (`seed` = terminal words, `branch` = per-word bits), но `codec.rs` ещё использует старую.

### 3. `analyze_strict_window_candidate` считает `floor_bits` по старой формуле

```rust
let breadcrumb_bits = word_count as u64 * steps as u64;
let floor_bits = key.bit_len as u64 + breadcrumb_bits + OPERATOR_BLOCK_FIXED_OVERHEAD_BITS + STRICT_OPERATOR_MIN_TERMINAL_BITS;
```

`word_count * steps` — это количество breadcrumb бит в биективной системе. В collapse-системе будет `word_count * branch_rounds` (из `PackedConstantK`) плюс seed_bits (фиксированные). Пока `StrictOperatorKey` не заменён на `PackedConstantK`, window selection оценивает экономику неверно и может отбрасывать окна, которые дали бы сжатие.

### 4. `compile_topology_to_constant` — публичная, но не вызывается

`compile_topology_to_constant` объявлена как `pub fn` в `topology.rs` и **не под `#[cfg(test)]`**.  Это уже production-функция. Но в `codec.rs` она не вызывается нигде — `try_encode_operator_block` вызывает `compile_strict_operator_key` вместо неё. То есть всё готово на уровне topology, но шов не закрыт.

***

## Как это пофиксить — конкретно по шагам

### Шаг 1: Починить `BitBudget` несоответствие

В `codec.rs` найти все места где создаётся `BitBudget` с полем `crumb_bits` и разбить его на `seed_bits` (terminal payload) и `branch_bits` (breadcrumbs):

```rust
// operator_budget() — было:
crumb_bits: ((operator.breadcrumbs.len() + operator.terminal_payload.len()) * 8) as u64,

// стало:
seed_bits: (operator.terminal_payload.len() * 8) as u64,
branch_bits: (operator.breadcrumbs.len() * 8) as u64,
```

То же самое в `try_encode_trajectory_block`. Это сначала просто чинит компиляцию.

### Шаг 2: Вывести `contract_block` / `materialize_runtime` из `#[cfg(test)]`

В `operator.rs` снять `#[cfg(test)]` с `contract_block`, `expand_block`, `materialize_runtime`. Они должны быть production-функциями.

### Шаг 3: Переключить `try_encode_operator_block` на `compile_topology_to_constant`

```rust
// Было:
let key = compile_strict_operator_key(&signature, block.len() * 8)?;
let steps = compile_strict_operator_steps(&signature, block.len() * 8)?;
let breadcrumbs = apply_strict_operator_schedule(&mut words, key.bit_len, &key.bytes, steps)?;

// Стало:
let packed_k = compile_topology_to_constant(&signature, block.len() * 8)?;
let runtime = materialize_runtime(&packed_k)?;
let layout = packed_k.layout()?;
let (seed, branches) = contract_block(&mut words, &runtime)?;
```

Wire format: `key` = `packed_k.as_bytes()`, `terminal_payload` = seed, `breadcrumbs` = branches.

### Шаг 4: Переключить `decode_operator_block` на `expand_block`

```rust
// Вместо apply_strict_operator_inverse_step в цикле:
let packed_k = PackedConstantK::parse(&block.key)?;
let runtime = materialize_runtime(&packed_k)?;
let layout = packed_k.layout()?;
expand_block(&mut words, &block.breadcrumbs, &runtime)?;
```


### Шаг 5: Обновить `analyze_strict_window_candidate`

`floor_bits` пересчитать через `branch_rounds` из `PackedConstantK`:

```rust
let layout = compile_topology_to_constant(&signature, block.len() * 8)?.layout()?;
let branch_bits = word_count as u64 * layout.branch_rounds as u64;
let seed_bits = 64_u64; // или реальный размер seed
let floor_bits = packed_key_bits + branch_bits + seed_bits + OVERHEAD;
```


### Шаг 6: Обновить `operator_budget` под новую семантику

`crumb_bits` теперь `seed_bits + branch_bits` раздельно — `CompressionPolicy::accepts` должен видеть реальную структуру, а не суммарный blob.

***

## Итоговая карта пробелов

| Пробел | Файл | Эффект |
| :-- | :-- | :-- |
| `BitBudget.crumb_bits` → `seed_bits + branch_bits` несоответствие | `codec.rs` vs `budget.rs` | Не компилируется или считает неверно |
| Биективные breadcrumbs в wire format | `codec.rs` | ~0% сжатие оператором |
| `compile_topology_to_constant` не вызывается в production | `codec.rs` | Collapse-путь мёртв |
| `contract_block`/`expand_block` под `#[cfg(test)]` | `operator.rs` | Нет production codec пути |
| `floor_bits` формула для старого steps | `codec.rs` | Window selection отбрасывает хорошие окна |
| `decode_operator_block` не использует `expand_block` | `codec.rs` | Декодер несовместим с будущим encoder |

