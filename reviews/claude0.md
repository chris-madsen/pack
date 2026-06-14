
# Он короче мне пишет:

```
Ревью частично устарело: главный баг про незаполненный dominant_walsh_mask уже закрыт. Сейчас я сфокусируюсь на оставшихся реальных смысловых дырах: мёртвые поля K/routing и неиспользованный word-level peak analyzer
```

Ты подтянул из мастера, прежде чем делать ревью?
Если подтянул, то как ему разъеб устроить?

Всё, теперь у меня свежий `topology.rs`. Вот прямой ответ.

***

# Да — он прав. Ты облажался на ревью

Разбор конкретно.

## Что он исправил и я не увидел

В `topology.rs` строки:

```rust
dominant_walsh_mask: signature.word_parity.mask,
dominant_walsh_bias: signature.word_parity.bias,
```

Это **есть** в мастере. И даже тест это явно покрывает:

```rust
#[test]
fn word_local_parity_feature_is_propagated_into_the_runtime_key() {
    ...
    assert_eq!(layout.dominant_walsh_mask, signature.word_parity.mask);
    assert_eq!(layout.dominant_walsh_bias, signature.word_parity.bias);
}
```

Я написал "Главный незакрытый баг — `compile_topology_to_constant` не заполняет `dominant_walsh_mask`" — это было **неверно**. Он закрыт. Ревью по этому пункту — мимо.

***

## Как устроить разбор — с конкретными строками

Три реальных смысловых проблемы которые **действительно есть** в коде:

### 1. `word_parity.mask` — это не Walsh-маска блока, а word-level parity маска. Это разные вещи математически

`strongest_word_parity_feature` перебирает маски вида `1 << bit` и `0x0101...01 << bit` (72 маски), считает для каждой сколько слов имеют нечётный popcount под маской, берёт максимум.  Это **не Walsh-спектр блока** — это статистика по независимым словам.

Walsh-спектр (`integer_fwht`) работает по **всем битам блока как единому вектору** и даёт коэффициент линейной зависимости между позициями внутри каждого слова. `word_parity` даёт независимую статистику по словам как целым. Это разные пространства — блок-Walsh vs. word-parity. Один не является аппроксимацией другого. Если данные имеют межсловную периодичность (напр. ASCII текст где каждый 8-й бит 0), `word_parity` поймает это. Но если структура внутрисловная — Walsh-пик из `walsh_peaks[0]` был бы корректнее. Сейчас `walsh_peaks` вычисляется, в `compile_spectral_key` используется — но в `compile_topology_to_constant` **игнорируется полностью**. Сюда стоит зайти.

### 2. Routing (`Butterfly`, `RotateWords`, etc.) сериализуется в K, но в `contract_block` и `expand_block` не применяется — мёртвый код

Посмотри `contract_block` в `operator.rs`:

```rust
pub fn contract_block(runtime: &RuntimePipeline, block: &[u8]) -> Result<(Vec<u8>, Vec<u8>), String> {
    ...
    let mut words = block.chunks_exact(8).map(...).collect::<Vec<_>>();
    // routing: вообще нет ни одного вызова
    for round in 0..runtime.layout.branch_rounds as usize {
        let geometry = branch_geometry(&runtime.layout, round, live_bits)?;
        ...
    }
```

`runtime.layout.routing` нигде не используется в `contract_block` и `expand_block`. Поля `rotate_left`, `rotate_right`, `gf2_shift`, `lane_rotate`, `pivot_seed`, `branch_stride`, `branch_span` тоже — они сериализуются в `PackedConstantK` и занимают байты в ключе, но не влияют ни на что в runtime. Компрессор тратит биты ключа на параметры которые не делают ничего.

### 3. `branch_rounds = word_parity.mask.count_ones().clamp(1, 16)` — это не адаптивная оптимизация, это максимально возможное значение

Строка:

```rust
let branch_rounds = signature.word_parity.mask.count_ones().clamp(1, 16) as u8;
```

`count_ones()` маски — это количество независимых pivot-позиций. Брать их все — значит убирать максимально возможное количество бит из каждого слова. Но `BitBudget.accepts()` принимает блок только если `encoded_bits < source_bits`.  Больше раундов → больше seed экономии (каждое слово на 1 бит короче), но и больше crumbs. Оптимальное число раундов зависит от плотности crumbs — и оно не обязательно максимальное. При плотных crumbs (шумные данные) один лишний раунд может сделать `BitBudget.accepts() → false`. Нужна итерация: пробовать 1..=max, считать budget, брать лучший.

***

## Итого для разбора

Скажи ему прямо: **по пункту 3 (topology → dominant_walsh_mask) ревью было неверным** — я не читал свежий `topology.rs` и написал о баге который уже закрыт. Это факт, не оправдание.

Реальные дыры которые остались и которые он сам упомянул ("мёртвые поля K/routing и неиспользованный word-level peak analyzer") — это именно пункты 2 и 1 выше, и они реальны.

