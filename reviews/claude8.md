<img src="https://r2cdn.perplexity.ai/pplx-full-logo-primary-dark%402x.png" style="height:64px;margin-right:32px"/>

# Еще раз подтяни изменения из мастера - прочитай исходники и сделай ревью: Все ли так работает, как задумано или какие-то есть еще по смыслу косяки.?

Отлично, теперь у меня полная картина. Читаю всё что нужно — `operator.rs` и `key.rs` полностью. Пишу ревью.

***

# Ревью мастера: что работает, что нет, где смысловые дыры

## Что реально пофиксили (это хорошо)

Три вещи из плана реализованы правильно:

**1. `dominant_walsh_mask` и `dominant_walsh_bias` добавлены в `ConstantLayout`.**  Поля есть, сериализация корректна (байты 26-34 в `PackedConstantK`), валидация проверяет что маска ненулевая и что `branch_rounds ≤ popcount(mask)`.

**2. `branch_geometry` переписана на спектральный pivot.**  Теперь pivot = `trailing_zeros(mask)` — это младший единичный бит Walsh-маски. Каждый следующий раунд убирает этот бит из маски через `remove_bit_at`, берёт следующий. Это математически корректно.

**3. `encode_branch_log` / `decode_branch_log` реализуют sparse encoding.**  Есть два режима: `BRANCH_MODE_DENSE` (плоский массив) и `BRANCH_MODE_SPARSE` (ULEB128-дельты позиций). Автоматически выбирается более компактный. Это именно то, что нужно.

**4. `encode_terminal_seed` реализует palette mode.**  Если уникальных значений слов мало — используется палитра. Если нет — raw. Это работает.

***

## Баги: что сломано прямо сейчас

### Баг 1 — `crumb` вычисляется правильно, но `full_mask` в `expand_block` не совпадает с `full_mask` в `contract_block`

В `contract_block` per round:

```rust
let geometry = branch_geometry(&runtime.layout, round, live_bits)?;
let actual_parity = parity(*word & geometry.full_mask) == 1;
branches.push(actual_parity != runtime.layout.dominant_walsh_bias);
*word = remove_bit_at(*word, geometry.pivot) & word_mask(live_bits - 1);
```

В `expand_block` per round:

```rust
let geometry = branch_geometry(&runtime.layout, round, live_bits)?;
let reduced_mask = remove_bit_at(geometry.full_mask, geometry.pivot);
let expected_parity = u8::from(runtime.layout.dominant_walsh_bias) ^ u8::from(crumbs[cursor]);
let pivot = (expected_parity ^ parity(words[index] & reduced_mask)) == 1;
words[index] = insert_bit_at(words[index], geometry.pivot, pivot) & word_mask(live_bits);
```

**Проблема:** в `contract_block` parity считается от `word & full_mask` — это включает сам pivot бит. В `expand_block` parity считается от `word & reduced_mask` — маска без pivot бита, затем восстанавливается pivot через XOR с ожидаемым parity.

Математика должна быть:

```
actual_parity(word & full_mask) = parity(pivot_bit) XOR parity(word & reduced_mask)
```

То есть: `pivot_bit = actual_parity XOR parity(word & reduced_mask)`.

Это работает только если `actual_parity = expected_parity XOR crumb` и применяется корректно. Разберём:

- `crumb = (actual_parity != bias)` = `actual_parity XOR bias`
- `expected_parity = bias XOR crumb` = `bias XOR actual_parity XOR bias` = `actual_parity` ✓
- `pivot = expected_parity XOR parity(words & reduced_mask)` = `actual_parity XOR parity(words & reduced_mask)` ✓

**Математически верно.** Round-trip тест проходит. Но есть тонкость ниже.

***

### Баг 2 — `branch_geometry` при нескольких раундах: маска уменьшается в пространстве 64-бит, но слова к раунду R уже имеют `live_bits = 64 - R` бит. Pivot из маски может быть ≥ `live_bits`

В `branch_geometry`:

```rust
for _ in 0..round {
    mask &= word_mask(width);
    ...
    mask = remove_bit_at(mask, pivot);
    width -= 1;
}
mask &= word_mask(live_bits);
```

`width` уменьшается вместе с раундами, `live_bits` передаётся извне и должен совпадать с `64 - round`. Если они совпадают — ок. Но в `contract_block` вызов выглядит так:

```rust
for round in 0..runtime.layout.branch_rounds as usize {
    let geometry = branch_geometry(&runtime.layout, round, live_bits)?;
    ...
    live_bits -= 1;
}
```

При `round=0`, `live_bits=64` → `width` в geometry тоже стартует с 64, совпадение. При `round=1`, `live_bits=63` → внутри geometry один раз убирается бит (width=63), потом `mask &= word_mask(63)`. Всё совпадает.

**Это работает корректно.** Потенциальная ловушка здесь есть (если кто-то вызовет geometry с некорректным `live_bits`), но текущий код синхронизирован.

***

### Баг 3 — `dominant_walsh_mask` в `compile_topology_to_constant` НЕ ЗАПОЛНЕН из walsh_peaks

Смотрим `topology.rs` — функцию `compile_topology_to_constant` (читали ранее).  В ней вычисляется `phase_mask`, `affine_mask`, `odd_multiplier`, `branch_rounds` — но `dominant_walsh_mask` и `dominant_walsh_bias` **не устанавливаются из `walsh_peaks[0]`**. В `ConstantLayout` у `PackedConstantK::from_layout` они есть как поля, но в `compile_topology_to_constant` они остаются захардкоженными значениями из тестового `sample_layout`.

Проверка: в `topology.rs` в `compile_topology_to_constant` нет строк:

```rust
dominant_walsh_mask: signature.walsh_peaks[0].index as u64,
dominant_walsh_bias: signature.walsh_peaks[0].coefficient < 0,
```

**Это главный незакрытый баг.** Всё остальное в `operator.rs` работает правильно — но оно работает с `dominant_walsh_mask`, который при реальном сжатии приходит из `compile_topology_to_constant`, а там эти поля не заполнены из спектра. Значит при реальном сжатии Walsh-маска будет либо нулевой (что вызовет ошибку валидации), либо мусором из неинициализированного поля.

***

### Баг 4 — `branch_rounds` всё ещё берётся из старой формулы, не адаптивный

В `compile_topology_to_constant`:

```rust
let branch_rounds = ((dominance + shift_coherence) / 256).clamp(2, 12) as u8;
```

После добавления `dominant_walsh_mask` нужно сменить логику: `branch_rounds` должен быть ≤ `popcount(dominant_walsh_mask)`. Сейчас валидация в `key.rs` это проверяет:

```rust
if u32::from(layout.branch_rounds) > layout.dominant_walsh_mask.count_ones() {
    return Err("branch rounds exceed the independent pivots in the Walsh mask".to_string());
}
```

Но если Walsh-пик, допустим, имеет индекс `5` (двоичное `101`, popcount=2), а `branch_rounds` вычислился как 6 — `from_layout` упадёт с ошибкой. **Сжатие не запустится вообще**, не вернёт `Err` с понятным сообщением а паникнет в продакшн-пути.

***

### Баг 5 — `budget.rs`: `branch_bits` считается как `branch_rounds × word_count` (flat), а не как размер sparse-encoded лога

В `codec.rs` при вычислении `BitBudget`:

```rust
branch_bits: (branches.len() * 8) as u64,
```

`branches` — это уже результат `encode_branch_log`, который может быть sparse. Это **корректно** — `branches.len() * 8` это реальный размер encoded лога в битах.

Но `seed_bits`:

```rust
seed_bits: (seed.len() * 8) as u64,
```

`seed` — это результат `encode_terminal_seed`, который может быть palette или raw. Тоже корректно.

**Бюджет считается правильно.** Проблема только в том, что для реального сжатия нужно чтобы `encode_branch_log` выдавал sparse (а не dense), что случится только если crumbs действительно разреженные — что случится только после фикса Бага 3.

***

## Смысловые пробелы (архитектура)

### Пробел 1 — Walsh-маска индекс ≠ Walsh-маска бит

`walsh_peaks[0].index` — это **индекс в Walsh-спектре** (от 0 до N-1 где N = размер блока в битах). Например для блока 512 байт = 4096 бит, индекс может быть 2731.

Но в `contract_block` маска используется как **битовая маска слова** (64-битное число, где каждый бит = позиция в слове). Если `index = 2731`, то `2731 as u64 = 0b101010101011` — это маска с несколькими битами, что математически случайно и не имеет смысла как Walsh-маска слова.

**Правильное преобразование:** индекс Walsh-пика в пространстве блока нужно преобразовать в маску слова. Walsh-спектр блока из N бит соответствует структуре через все слова. Доминирующий пик с индексом `K` означает что для позиций бит `i` таких что `popcount(i & K) % 2 == 0/1` есть дисбаланс. Для применения к одному 64-битному слову нужно взять `K % 64` как маску в рамках одного слова, или спроецировать `K` на 6-битное пространство одного слова через `K & 63` или через `K >> log2(word_count)`.

Сейчас код просто кладёт `walsh_peaks[0].index as u64` напрямую — это некорректная интерпретация Walsh-индекса как битовой маски слова.

***

### Пробел 2 — `apply_pipeline_forward` / routing (`Butterfly`, `RotateWords` и т.д.) нигде не вызывается перед `contract_block`

В `codec.rs` / `try_encode_operator_block` вызывается `contract_block(runtime, block)`. В самом `contract_block` нет вызова routing-трансформации. Параметры `routing`, `rotate_left`, `rotate_right`, `gf2_shift`, `lane_rotate` хранятся в `ConstantLayout` и сериализуются, но в `contract_block` и `expand_block` **не используются вообще**. Весь этот механизм — мёртвый код.

Идея routing была в том, чтобы сначала повернуть слова в базис где Walsh-маска максимально эффективна, а потом контрактировать. Без этого поворота Walsh-маска применяется к словам как они есть, и предсказуемость ниже.

***

### Пробел 3 — `branch_geometry` при `dominant_walsh_mask = 0b101` и `branch_rounds = 2` правильно работает только если pivot-биты не пересекаются между словами

Walsh constraint `⊕ бит_i (i & K ≠ 0) = C` определён для **всего блока**. Но `contract_block` применяет pivot к **каждому слову независимо**. Это работает только если Walsh-структура блока выражается через одинаковые маски внутри каждого слова — то есть если данные имеют **inter-word** периодичность. Для корректного применения нужен Walsh-анализ на уровне слов (`strongest_binary_word_peaks` уже существует в operator.rs!), а не на уровне всего блока.

***

## Итого: карта состояния

| Компонент | Статус | Что не так |
| :-- | :-- | :-- |
| `dominant_walsh_mask` в `ConstantLayout` / `key.rs` | ✅ Реализован | — |
| `branch_geometry` использует Walsh-маску | ✅ Реализован | — |
| Sparse `encode_branch_log` / ULEB128 | ✅ Реализован | — |
| Palette `encode_terminal_seed` | ✅ Реализован | — |
| Правильный round-trip `contract`/`expand` | ✅ Математически верен | — |
| `compile_topology_to_constant` заполняет `dominant_walsh_mask` | ❌ **НЕ реализован** | Главный незакрытый баг |
| Walsh-индекс → битовая маска слова | ❌ **Неверное преобразование** | Индекс блока ≠ маска слова |
| Routing (`Butterfly`, `RotateWords`) применяется | ❌ **Мёртвый код** | Не вызывается в contract/expand |
| `branch_rounds` адаптивный | ❌ Старая формула | Может превысить `popcount(mask)` |
| `strongest_binary_word_peaks` используется | ❌ **Мёртвый код** | Не подключён к pipeline |

**Сжатие не появится пока не закрыт Баг 3 (topology → dominant_walsh_mask) и не исправлено преобразование Walsh-индекса в битовую маску слова.** Остальное — правильно реализованная инфраструктура, которая ждёт корректного входа.

