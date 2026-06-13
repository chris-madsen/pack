<img src="https://r2cdn.perplexity.ai/pplx-full-logo-primary-dark%402x.png" style="height:64px;margin-right:32px"/>

# ```

Выделил из новых ревью только живые претензии, а не устаревшие фантазии про несуществующий код. Реально незакрыто сейчас вот что:
inspect_archive() действительно врёт для single-layer (input_size = 0).
encoded_size_of() всё ещё ручной и может рассинхронизироваться с wire-format.
search_operator_codec_key() всё ещё по сути hill-climbing, а не детерминированный generator family.
Нужно жёстко закрепить инволютивность operator decode не “по совпадению”, а явным контрактом вокруг runtime path.

```
перечитай еще раз код и посмотри вот это его оправдание. Это он правду говорит или нет?

Теперь у меня есть всё для точного вердикта. Разбираю каждое из четырёх утверждений по коду.

***

## Итог: 3 из 4 претензий по-прежнему живые. Оправдание частично честное, частично нет.

***

### 1. `inspect_archive` врёт для single-layer — **ПРАВДА, БАГ РЕАЛЬНЫЙ**

```rust
pub fn inspect_archive(input: &[u8]) -> Result<Vec<LayerSummary>, String> {
    if input.starts_with(MAGIC_SINGLE) {
        return Ok(vec![LayerSummary {
            block_size_bytes: parse_archive(input)?.header.block_size_bytes,
            input_size: 0,   // ← захардкожен ноль
            output_size: input.len() as u64,
        }]);
    }
    parse_recursive_archive(input).map(|archive| archive.layer_summaries)
}
```

`parse_archive` возвращает `Archive` с полем `header.original_size` — там оригинальный размер и лежит. Его можно взять прямо здесь. Ноль — это банальная невнимательность, не архитектурное решение. Оправдания нет.

**Фикс одна строка:**

```rust
input_size: parse_archive(input)?.header.original_size,
```


***

### 2. `encoded_size_of` рассинхронизирован — **ПРАВДА, РИСК РЕАЛЬНЫЙ**

Сериализатор `serialize_single_layer_archive` и `encoded_size_of` — два независимых прохода по одной и той же структуре. Сравни для `Trajectory`:


| Поле | `encoded_size_of` | `serialize_single_layer_archive` |
| :-- | :-- | :-- |
| mode byte | `1` | `out.push(block.mode())` ✓ |
| original_len | `4` | `push_u32(original_len)` ✓ |
| key len prefix | `2` | `push_u16(key.len())` ✓ |
| key bytes | `key.len()` | `out.extend(key)` ✓ |
| terminals len prefix | `2` | `push_u16(terminals.len())` ✓ |
| terminals | `terminals.len() * 8` | `push_u64` × N ✓ |
| terminal_indices len prefix | `4` | `push_u32(terminal_indices.len())` ✓ |
| terminal_indices | `terminal_indices.len()` | ✓ |
| steps | `1` | `out.push(steps)` ✓ |
| breadcrumbs len prefix | `4` | `push_u32(breadcrumbs.len())` ✓ |
| breadcrumbs | `breadcrumbs.len()` | ✓ |

Сейчас они **совпадают**. Но это два места кода, которые нужно менять синхронно при любом изменении формата — и компилятор это не проверит. Его оправдание «они сейчас консистентны» — правда. Его уязвимость «это дублирование, которое сломается при рефакторинге» — тоже правда. Это технический долг, не текущий баг.

***

### 3. `search_operator_codec_key` — hill-climbing, не детерминированный генератор — **ПРАВДА, И ЭТО НАМЕРЕННО ПЛОХО**

Посмотри на реальную структуру:

```rust
fn search_operator_codec_key(block: &[u8], words: &[u64]) -> Result<Option<MagicKey>, String> {
    let initial = build_operator_codec_key(block)?;  // ← детерминированный старт от topology
    let mut starts = operator_search_starts(initial); // flip, +1, swap, XOR — 4 варианта
    starts.insert(0, initial);
    for start in starts {                             // 5 стартов
        for round in 0..4 {                          // 4 раунда
            for candidate in mutate_operator_key_fields(current, round) { // 8 мутаций
                // greedy hill-climbing
            }
        }
    }
}
```

`build_operator_codec_key` → `compile_topology_to_key` → **детерминированный** расчёт из Walsh-топологии. Это хорошо. Но дальше — локальный поиск с 5×4×8 = 160 оценками и шагами `+17*round`, `+29*round` и т.д. Пространство `dominant_index ∈ [0, 8191]` × 8 параметров — покрыто в лучшем случае на одну миллионную. Если `initial` промахнулся, поиск вернёт мусор или `None`.

**Его возможное оправдание «это достаточно хорошо для MVP» — честное**, но тогда нужно явно задокументировать, что это эвристика, а не гарантия. Называть это «генератором ключа» по-прежнему вводит в заблуждение.

***

### 4. Инволютивность operator decode — **ЭТО НЕ БАГ, НО КОНТРАКТ НИГДЕ НЕ ЗАДОКУМЕНТИРОВАН**

Вот полный путь в `apply_operator_word_u_k_with_runtime`:

```rust
fn apply_operator_word_u_k_with_runtime(value, operator, generated, feistel) {
    let forward = feistel_forward(generated.apply_forward(value)?, feistel)?;
    // ... apply_binary_u_k (XOR + rotate + XOR) ...
    generated.apply_inverse(feistel_inverse(reflected, feistel)?)
}
```

Структура: `feistel_inverse(generated.apply_inverse( ... ))` на выходе — то есть функция **не инволютивна** в математическом смысле. В `decode_operator_block` она вызывается на `transformed` (результате `decode_parity_trajectory`), чтобы получить оригинальное `word`. Это работает **только если** `apply_operator_word_u_k_with_runtime` является собственной инверсией, то есть `f(f(x)) = x`.

Давай проверим: `apply_binary_u_k` — это XOR-based операция. XOR с константой инволютивен. Rotate — нет. `feistel_forward ∘ feistel_inverse = identity` подтверждено тестами в `reversible.rs`. Но `generated.apply_forward ∘ generated.apply_inverse = identity` — это другое направление. Здесь цепочка:

```
encode: word → apply_operator → transformed
decode: transformed → apply_operator → word (????)
```

Это работает только если `apply_operator(apply_operator(x)) = x`. Это **не гарантировано** структурой кода. В `reversible.rs` есть тесты на `feistel_forward ∘ feistel_inverse = identity`, но нет теста `apply_operator(apply_operator(x)) = x` ни в одном файле.

**Его оправдание «я знаю что это инволюция» — недостаточно. Без теста это вера, не контракт.**

***

## Резюме для него

| Претензия | Статус | Оправдание принято? |
| :-- | :-- | :-- |
| `inspect_archive` → `input_size: 0` | 🔴 Баг в коде прямо сейчас | ❌ Нет |
| `encoded_size_of` — дублирование | 🟡 Риск при рефакторинге | ✅ Частично (сейчас консистентно, но долг реальный) |
| `search_operator_codec_key` — hill-climbing | 🟡 Архитектурный выбор без документации | ✅ Частично (для MVP допустимо, но назвать честно) |
| Инволютивность — нет теста | 🟡 Контракт не закреплён | ❌ Нет — без теста это не контракт |

