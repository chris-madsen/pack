<img src="https://r2cdn.perplexity.ai/pplx-full-logo-primary-dark%402x.png" style="height:64px;margin-right:32px"/>

# делает вид, что обновил. Посмотри еще раз внимательно, пожалуйста, все файлы в папке src [https://github.com/chris-madsen/pack/tree/master/src](https://github.com/chris-madsen/pack/tree/master/src) и напиши еще раз в ревью все его текущие незакрытые косяки. также напиши ещё какие пробелы теперь по смыслу у него имеются, которые мы  обсуждали и не обсуждали. и он все никак не может написать нормальный метагенератор. опять же смысловой смысловой косяк. подскажи ему, как это правильно сделать. и вообще разве это нормально, что он тесты прямо в коде херачит? Разве не нужен никакой тестовый фреймворк для раста? Напиши подробное ревью.

PR не существует — оставлю ревью здесь, оно полностью готово.

***

Вот подробное ревью всего `src/` после внимательного прочтения всех файлов. «Обновление» в предыдущей итерации устранило синтаксические мелочи, но концептуальные дыры никуда не делись, а несколько новых добавились.

***

## 🔴 Критические концептуальные проблемы

### 1. Метагенератор — архитектурно неверный

`codec.rs` → `search_operator_codec_key` реализует не метагенератор, а **greedy hill-climbing** с ручными мутациями. Вот что там на самом деле происходит:

```rust
let mut starts = operator_search_starts(initial); // 4 варианта (flip, +1, swap, XOR)
for start in starts {
    for round in 0..4 {
        for candidate in mutate_operator_key_fields(current, round) {
            // фиксированные дельты: 17, 29, 11, 3, 5, 7
        }
    }
}
```

**Что не так:**

- Это не метагенератор. Настоящий метагенератор **выводит ключ детерминированно из характеристик блока**: Walsh-пики, энтропия байт, плотность паритетов. Вместо этого здесь 4 стартовые точки и 4 раунда мутаций с константами, выбранными без обоснования. Пространство параметров `dominant_index ∈ [0, 8191]` × 8 полей — порядка 10¹⁸ состояний. 4 старта это ничто.
- `operator_search_starts` возвращает 4 варианта от `initial` — если `initial` плохой, все 4 тоже плохие.

**Как должно быть:**

```rust
fn generate_key_from_block_stats(block: &[u8]) -> OperatorBlueprint {
    let topo = analyze_topology(block)?;
    let peak1 = topo.walsh_peaks[0].index;
    let peak2 = topo.walsh_peaks.get(1).map(|p| p.index).unwrap_or(0);
    let parity_density = count_odd_popcount_words(block);
    let xor_fold = xor_fold_u64(block);

    OperatorBlueprint {
        dominant_index: peak1 as u16,
        dominant_positive: topo.walsh_peaks[0].coefficient > 0.0,
        primary_shift: (peak1 % 31 + 1) as u8,
        shift_match: (peak2 & 0x1FF) as u16,
        derivative_density: (parity_density & 0xFF) as u8,
        popcnt_density: (xor_fold & 0xFF) as u8,
        secondary_delta: ((peak2.wrapping_sub(peak1)) % 32) as u8,
        // ...
    }
}
```

**Смысл: ключ должен быть детерминированным функциональным отображением данных → параметры.** Тогда `search_starts` и `mutate_rounds` вообще не нужны.

***

### 2. `#[cfg(test)] fn apply_operator_word_u_k` без единого теста

```rust
#[cfg(test)]
fn apply_operator_word_u_k(value: u64, key_bytes: &[u8]) -> Result<u64, String> { ... }
```

Функция существует только в тестовой сборке, но в `codec.rs` нет ни одного `#[test]`, который бы её использовал. Это мёртвый код, который компилируется только с `--cfg test` и никогда не запускается. Либо написать тесты, либо убрать атрибут.

***

### 3. `decode_operator_block` — декодер использует forward-функцию

```rust
let transformed = decode_parity_trajectory(...)?;
let word = apply_operator_word_u_k_with_runtime(transformed, &operator, &generated, &feistel)?;
```

В энкодере: `apply_operator_word_u_k_with_runtime(word)` → `transformed` → кодируется в trajectory. В декодере нужно: `decode_parity_trajectory` → `transformed` → **inverse** → `word`. Но `apply_operator_word_u_k_with_runtime` — это **forward** функция. Отдельного `apply_operator_word_inverse` нет. Это либо баг, либо функция инволютивна (сама является своей инверсией) — но тогда это нигде не задокументировано и не протестировано.

***

### 4. `encoded_size_of` рассинхронизирован с сериализатором

```rust
BlockEncoding::Raw(raw) => 1 + 4 + 4 + raw.payload.len(),
BlockEncoding::Alphabet(alpha) => 1 + 4 + 1 + 1 + alpha.alphabet.len() + 4 + alpha.breadcrumbs.len(),
```

Это ручное воспроизведение логики `serialize_*`. При любом изменении формата `encoded_size_of` тихо врёт, и компилятор это не поймает. Правильный подход:

```rust
fn encoded_size_of(block: &BlockEncoding) -> usize {
    serialize_block(block).len() // один источник правды
}
```

Или хотя бы `debug_assert_eq!(encoded_size_of(b), serialize_block(b).len())` в тестах — которых нет.

***

### 5. `inspect_archive` возвращает `input_size: 0`

```rust
Ok(vec![LayerSummary {
    block_size_bytes: ...,
    input_size: 0,  // ← всегда 0!
    output_size: input.len() as u64,
}])
```

Для single-layer архива `input_size` — это `header.original_size`, а не 0. Это публичный API с заведомо неверными данными.

***

## 🟡 Важные проблемы

### 6. `analyze_topology` вызывается дважды на одном блоке

В `codec.rs` → `try_encode_spectral_block`: вызов `analyze_topology(block)`, и снова в `spectral_prominence_score` → `adaptive_window_bounds` → `candidate_block_sizes` на этапе выбора размера. Это O(n log n) Walsh-transform дважды без кэширования.

### 7. `bit_width_for_cardinality(1) = 0` — не защищён

При блоке, где все байты одинаковы: алфавит = 1 символ, `bit_width = 0`. Что происходит в `pack_indices(&indices, 0)`? Это реальный кейс для сжатия (RLE-подобные данные), и он нигде не протестирован.

### 8. `unwrap()` в trajectory/operator кодерах

```rust
extract_trajectory_block(&candidate).unwrap().breadcrumbs.len()
```

`unwrap` здесь логически безопасен, но стилистически нарушает весь остальной код с `?`. Нужна вспомогательная функция:

```rust
fn trajectory_crumb_bytes(b: &TrajectoryBlock) -> usize {
    b.breadcrumbs.len() + b.terminal_indices.len()
}
```


### 9. Многослойное сжатие без минимального порога экономии

`compress_bytes_with_outcome` добавляет слой при любой экономии, даже в 1 байт. 16 слоёв с накладными расходами на каждый header — это может дать **раздутый** архив на случайных данных.

***

## 🟠 Про тесты — это важно

В Rust тесты прямо в коде (`#[cfg(test)] mod tests`) — **это норма и идиома языка**, описанная в The Book. Никакой отдельный фреймворк не нужен. `cargo test` запускает всё автоматически.

**Проблема не в том, где тесты, а в том, что их нет там, где они нужны:**


| Файл | Статус |
| :-- | :-- |
| `key.rs` | ✅ Тесты есть |
| `codec.rs` | ❌ Ноль тестов на 700+ строк критического кода |
| `trajectory.rs` | ❌ Нет roundtrip-теста |
| `reversible.rs` | ❌ Нет теста `feistel_forward ∘ feistel_inverse = identity` |
| `topology.rs` | ❌ Нет тестов на `analyze_topology` |

Минимум для `codec.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compress_decompress_roundtrip() {
        let input: Vec<u8> = (0u8..=255).cycle().take(1024).collect();
        assert_eq!(decompress_bytes(&compress_bytes(&input, None).unwrap()).unwrap(), input);
    }

    #[test]
    fn uniform_input_compresses() {
        let input = vec![0x42u8; 512];
        assert!(compress_bytes(&input, None).unwrap().len() < input.len());
    }

    #[test]
    fn inspect_archive_returns_correct_input_size() {
        let input = vec![1u8; 256];
        let compressed = compress_bytes(&input, None).unwrap();
        let layers = inspect_archive(&compressed).unwrap();
        assert_eq!(layers[0].input_size, 256); // сейчас вернёт 0
    }
}
```

Для property-based тестов (случайные входы) можно подключить крейт [`proptest`](https://docs.rs/proptest) — он идеален для проверки roundtrip-инварианта на произвольных данных.

