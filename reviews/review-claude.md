<img src="https://r2cdn.perplexity.ai/pplx-full-logo-primary-dark%402x.png" style="height:64px;margin-right:32px"/>

# делает вид, что обновил. Посмотри еще раз внимательно, пожалуйста, все файлы в папке src. и напиши еще раз в ревью все его текущие незакрытые косяки. также напиши ещё какие пробелы по смыслу у него имеются, которые мы не обсуждали. и он все никак не может написать нормальный метагенератор. опять же смысловой смысловой косяк. подскажи ему, как это правильно сделать.

Теперь у меня есть полная картина. Пишу развёрнутое ревью.

***

# Code Review — commit `ae6567c` («Initial fixes»)

## Что реально улучшилось

Три вещи сделаны правильно и заслуживают признания.

**Спектральный encode-путь наконец существует.** `try_encode_spectral_block` теперь присутствует в `encode_block`, использует `compile_spectral_key` + `synthesise_spectral_bits` и кодирует исключения через ULEB128.  Тест `real_encode_path_emits_spectral_for_a_known_walsh_signal` проходит на синтетических Walsh-данных — это прогресс.

**MagicKey стал настоящей 64-битной магической константой.** `key.rs` полностью переписан: `MagicKey(u64)` хранит один `u64`, в котором упакованы до трёх Walsh-пиков (индекс 13 бит × 3, амплитуда 5 бит × 3, знак 1 бит × 3, peak\_count 2 бита, block\_log2 4 бита, tie\_bit 1 бит). `serialize()` возвращает `[u8; 8]`.  Это правильная архитектура для той части, которая касается спектрального ключа.

**Оператор теперь проверяет реальную пользу.** Добавлен `minimal_common_terminal_steps`, и `try_encode_operator_block` возвращает `None`, если U\_k не улучшила выравнивание terminal по сравнению с исходными словами.  Это закрывает ранее отмеченную дыру.

***

## Незакрытые и новые косяки

### 1. Два типа ключей в одной системе — фундаментальный разрыв

Система делает вид, что у неё один формат ключа — `MagicKey(u64)`. Но фактически есть **два семантически несовместимых использования одного типа**:

- **Spectral-ключ** (`from_spectral_program`): биты строго распределены по полям (пики, амплитуды, знаки, block\_log2). `spectral_program()` их правильно разворачивает.
- **Operator/Trajectory-ключ** (`from_raw`): хранит просто хеш блока (`avalanche(fingerprint ^ derivative_fold ^ ...)`). `parse_operator_runtime_key` разворачивает его через битовые сдвиги и арифметику.

Но `MagicKey::parse(bytes)` — один и тот же метод для обоих. Ничто не мешает передать operator-ключ в `spectral_program()` и получить мусорный SpectralProgram вместо ошибки. Именно это и происходит в `decode_trajectory_block`:

```rust
let key = MagicKey::parse(&block.key)?;
if key.raw() & 0x3F != block.steps as u64 {
    return Err(...);
}
```

Здесь `key.raw() & 0x3F` — это младшие 6 бит хеша.  Это не «шаги» в каком-либо осмысленном смысле — это случайные биты хеша, которые совпали со `steps` при построении ключа. Валидация в decode проверяет, что случайные биты хеша совпадают с полем в блоке, но не проверяет, является ли это корректным operator-ключом семантически.

**Как правильно:** `MagicKey` должен хранить тип (`SpectralKey` / `OperatorKey` / `TrajectoryKey`), например через enum, или хотя бы через выделенный tag-бит. Сейчас тип определяется контекстом вызова, а не самим ключом.

***

### 2. `apply_binary_u_k` — не U\_k, а суррогат, и его инволютивность не гарантирует сжатие

`apply_binary_u_k` делает:

1. `(value ^ xor_mask).rotate_left(rotate)` — XOR + ротация
2. `binary_hadamard_forward` — бинарный CNOT-circuit
3. XOR с `phase_mask`
4. Обратный Hadamard + ротация обратно

Это действительно инволюция (`u_k(u_k(x)) == x`), тест это подтверждает.  Но инволютивность здесь **случайное следствие симметрии**, а не инструмент сжатия.

Настоящий U\_k на вещественных значениях (`apply_u_k` в `operator.rs`) **не вызывается нигде в encode-пути** — он только в тестах.  `apply_operator_word_u_k_with_runtime` использует исключительно `apply_binary_u_k`. Вся математика FWHT на `f64` существует только как тест.

Ключевая проблема: `binary_hadamard_forward` реализует матрицу Hadamard над GF(2) (CNOT-circuit). Это **не то же самое**, что FWHT над вещественными числами. Walsh-анализ, которым строится ключ в `analyze_topology` (где FWHT реальный, на `f64`), несовместим с бинарным Hadamard, которым этот ключ применяется. Спектральный анализ блока и оператор применения живут в разных математических пространствах.

***

### 3. Spectral encode работает только на идеальных Walsh-сигналах

`try_encode_spectral_block` строит предиктор из 3 Walsh-пиков и кодирует исключения (биты, которые не совпали).  Для чистого Walsh-сигнала (тест `walsh_basis_bytes`) исключений 0, residual пуст, сжатие максимальное.

Для реальных данных (текст, бинарники, JSON): Walsh-спектр диффузный, пиков много, 3 пика дают предиктор с ~50% совпадением. Residual ≈ 50% бит — это **больше исходного блока**, а не меньше. `CompressionPolicy::MVP.accepts(budget)` с `minimum_gain_fraction: 0.10` отсеет это. Spectral-блок на реальных данных будет срабатывать редко или никогда.

Это не баг, а ограничение дизайна — но нигде не задокументировано и тест `roundtrip_ascii_and_binary_data` молчаливо проходит, скрывая реальное поведение. Тест не проверяет, какой тип блока был выбран.

***

### 4. `compile_topology_to_key` и `compile_spectral_key` строят **разные ключи** из одного сигнала — и ни один не объясняет, почему

Для одного и того же блока:

- `compile_spectral_key` — строит ключ, который буквально кодирует топ-3 Walsh-пика (индекс, знак, амплитуда)
- `compile_topology_to_key` — строит ключ как avalanche-хеш всей топологии (Walsh + derivative + popcnt + shift + fingerprint)

Обе функции публичные, обе возвращают `MagicKey`. Нигде не указано, что первая — только для Spectral-блоков, вторая — только для Operator-блоков. В `codec.rs` `compile_spectral_key` вызывается в `try_encode_spectral_block`, а `compile_topology_to_key` — в `build_operator_codec_key`.  Это случайно верно, но хрупко.

***

### 5. `spectral_program()` вызывается на operator-ключах в parse-пути

В `parse_archive`, блок mode=1 (Spectral):

```rust
let parsed = MagicKey::parse(&key)?;
let program = parsed.spectral_program()?;
if program.bit_len != original_len as usize * 8 { ... }
let predictor = synthesise_spectral_bytes(parsed)?;
apply_spectral_exceptions(&predictor, &residual, program.bit_len)?;
```

Здесь ключ сначала декодируется как SpectralProgram (проверяется block\_log2 и пики), затем тот же `parsed` передаётся в `synthesise_spectral_bytes`. Это работает для spectral-ключей. Но `synthesise_spectral_bytes` внутри снова вызывает `key.spectral_program()` — то есть декодирование происходит дважды.  Минорная неэффективность, но симптом того, что нет явного типа ключа.

***

### 6. `bit_len()` на `MagicKey` возвращает 64 — всегда, независимо от содержимого

```rust
pub fn bit_len(self) -> usize {
    u64::BITS as usize  // всегда 64
}
```

Метод называется `bit_len`, что подразумевает «длину закодированного блока». Но правильная длина блока хранится внутри ключа как `block_log2`. В тесте в `topology.rs`:

```rust
assert_eq!(first.bit_len(), 64);
```

Это проверяет размер самого ключа (64 бита), а не длину блока. Тест misleading — `first.bit_len()` здесь означает «ключ 64 бита», но читается как «блок 64 бита».

***

### 7. `generate_base_from_root` использует XORSHIFT-LFSR, который имеет период < 2^64

```rust
let mut state = root.value | 1;
for round in 0..8_u32 {
    state ^= state << 13;
    state ^= state >> 7;
    state = state.rotate_left((round % 31) + 1);
    ...
}
```

Тройной xorshift с rotate — это не плохо само по себе, но `state` не используется как прогрессивный генератор: каждая итерация мутирует один state независимо. `round_constant` тоже использует отдельный avalanche от root — то есть round\_constant(root, 0) и round\_constant(root, 1) детерминированы и независимы. Это нормально.

Проблема семантическая: `generate_base_from_root` выбирает **schedule из 8 операций** из 12 обратимых. Каждый раунд выбирает одну операцию по `state % reversible.len()`. Это означает, что одна и та же операция может выбраться 8 раз подряд, если `state % 12` стабилизировался. Нет гарантии разнообразия schedule. Для ключей с `root.value` близким к нулю schedule вырождается.

***

## Главный нераскрытый косяк: метагенератор неправильный по смыслу

Это самое важное, что нужно исправить. Разберём, что именно не так и как должно быть.

### Что сделано сейчас

`parse_operator_runtime_key` принимает 8-байтный ключ (он же хеш блока) и **разворачивает** из него все параметры операторов детерминированно через битовые сдвиги и арифметику:

```rust
let seed     = raw.rotate_left(17)  ^ 0xA076...;
let mask     = raw.rotate_right(9)  ^ 0xE703...;
let shift_l  = ((raw >> 7)  % 31 + 1) as u8;
let shift_r  = ((raw >> 23) % 31 + 1) as u8;
let odd_mul  = raw.rotate_left(29) | 1;
let generated = generate_base_from_root(&standard_base(1)?, RootSeed { value: raw ^ mask })?;
```

Это **раскрыватель ключа** (key expander): из одного u64 генерируются все параметры. Это нормальный паттерн.

### Где семантический провал

Метагенератор — это не просто «развернуть ключ в параметры». Метагенератор должен решить задачу **в обратную сторону**:

> Дан конкретный блок данных. Найди такой ключ K (64-битное число), при котором трансформация U_k(block) максимально улучшает сжатие.

Текущий поток работает так:

1. Взять блок → посчитать fingerprint → avalanche → `MagicKey`
2. Раскрыть ключ в параметры
3. Применить U\_k
4. Проверить, стало ли лучше

То есть ключ **не ищется**. Он просто вычисляется как детерминированный хеш блока. Это делает U\_k детерминированной, но не оптимальной. Вероятность того, что случайный хеш блока даёт хорошее выравнивание terminal — случайная.

### Как должен работать правильный метагенератор

Метагенератор выполняет **поиск в пространстве ключей**, руководствуясь сигналом качества:

```
score(K, block) = minimal_common_terminal_steps(U_k(block))
```

Чем больше score — тем лучше ключ. Нужно найти K с максимальным score.

Полный перебор 2^64 ключей невозможен. Правильная стратегия — **направленный поиск**:

1. **Инициализация:** взять Walsh-пики блока. Наиболее вероятный ключ — тот, который выравнивает доминирующую частоту. Начать с K₀ = fingerprint(block).
2. **Мутация ключа:** изменять отдельные битовые поля ключа (xor_mask, rotate, phase_mask) по одному, каждый раз измеряя score.
3. **Жадное восхождение:** принимать мутацию, если score не ухудшился. Отвергать иначе. Итерировать 32–64 раза (это не дорого, U\_k на блоке 64 слова — быстрая операция).
4. **Критерий остановки:** score(K, block) > score(K₀, block) — нашли улучшение. Или исчерпали лимит итераций — вернуть None.

Схема в коде:

```rust
fn search_operator_key(block: &[u8]) -> Option<MagicKey> {
    let words: Vec<u64> = block.chunks_exact(8)
        .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
        .collect();
    
    let baseline = minimal_common_terminal_steps(&words);
    let mut best_key = block_fingerprint(block).ok()?;
    let mut best_score = 0u8;
    
    // Попробовать несколько стартовых точек: fingerprint + Walsh-производные
    let candidates = spectral_key_candidates(block); // top-N Walsh конфигурации
    
    for start_key in candidates {
        let mut key = start_key;
        for _ in 0..64 {
            // Мутировать один диапазон бит ключа
            for bit_field in KEY_FIELDS {
                let mutated = mutate_field(key, bit_field);
                let transformed = apply_u_k_words(&words, mutated);
                let score = minimal_common_terminal_steps(&transformed).unwrap_or(0);
                if score > best_score {
                    best_score = score;
                    best_key = mutated;
                }
            }
        }
    }
    
    // Принять только если реально лучше исходного
    if baseline.is_some_and(|b| best_score >= b) {
        return None;
    }
    best_score.is_some().then_some(MagicKey::from_raw(best_key))
}
```

Вот чего нет в текущем коде и что является настоящим смыслом метагенератора. `block_fingerprint` → `avalanche` — это не поиск, это одна детерминированная ставка наудачу.

***

## Итог по приоритетам

| Проблема | Критичность |
| :-- | :-- |
| `MagicKey` не различает spectral/operator/trajectory типы по тегу | Высокая — хрупкость decoder |
| `apply_binary_u_k` несовместим с Walsh-анализом из `analyze_topology` | Высокая — математическая несвязность |
| Метагенератор не ищет ключ, а генерирует его хешем | Высокая — нулевая оптимизация |
| `bit_len()` возвращает размер ключа, не блока | Средняя — misleading API |
| Spectral encode бесполезен для реальных данных | Средняя — нет документации ограничений |
| Дублирование `spectral_program()` в parse-пути | Низкая — неэффективность |
| `generate_base_from_root` может вырождаться для ключей ~0 | Низкая — edge case |

