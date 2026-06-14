<img src="https://r2cdn.perplexity.ai/pplx-full-logo-primary-dark%402x.png" style="height:64px;margin-right:32px"/>

# Еще раз !подтяни изменения из мастера! - прочитай исходники src/ и сделай ревью: Все ли теперь так работает, как задумано или какие-то есть еще по смыслу косяки.?

Теперь у меня всё — полный `codec.rs`, `operator.rs` и `topology.rs` из актуального мастера. Делаю настоящее ревью.

***

# Ревью по актуальному мастеру

Прочитал всё: `codec.rs` (93 KB), `operator.rs` (44 KB), `topology.rs` (37 KB).  Вот честный вердикт.

***

## ✅ Что работает правильно

**Collapse-core в продакшне** — `contract_block` и `expand_block` — полноценные `pub fn` без `#[cfg(test)]`, вызываются из `try_encode_operator_block` и `decode_operator_block` в `codec.rs`.  Реальное удаление битов через `remove_bit_at`, обратное восстановление через `insert_bit_at`. Это работает.

**`dominant_walsh_mask` заполнен** — `compile_topology_to_constant` пишет `signature.word_parity.mask` и `signature.word_parity.bias` в `ConstantLayout`. Тест `word_local_parity_feature_is_propagated_into_the_runtime_key` это подтверждает.

**Branch log sparse/dense авто-выбор** — `encode_branch_log` честно сравнивает длины и выбирает меньший вариант.  Это корректно.

**Canonicity проверяется** — `decode_branch_log` re-encodes и сравнивает, `decode_operator_block` перегоняет через `contract_block` и сравнивает seed и branches.

**Routing реально применяется** — `apply_runtime_routing_forward` и `apply_runtime_routing_inverse` вызываются в `contract_block` / `expand_block`, обрабатывают `Identity`, `RotateWords`, `ReverseWords`, `Butterfly`.  Тест `runtime_routing_changes_operator_payload_and_preserve_roundtrip` проверяет, что разный routing даёт разный payload. Это **не мёртвый код** — предыдущее ревью (которое я давал ранее) тут было неверным.

***

## 🔴 Реальный смысловой косяк — `runtime_analysis_mask` против `dominant_walsh_mask`

Это настоящая незакрытая проблема. В `branch_geometry` → `runtime_analysis_mask`:

```rust
fn runtime_analysis_mask(layout: &ConstantLayout, round: usize, live_bits: usize) -> u64 {
    let live_mask = word_mask(live_bits);
    let base = layout.dominant_walsh_mask & live_mask;
    let family_mix = match layout.family { ... };
    let rotate = ((layout.rotate_left * (round+1)) + layout.rotate_right
        + layout.gf2_shift * layout.branch_stride
        + layout.pivot_seed + layout.branch_span) % 64;
    let mixed = (family_mix.rotate_left(rotate) ^ repeat_byte(...)) & live_mask;
    (base | mixed) & live_mask
}
```

`base` — это `word_parity.mask`, то есть маска, выбранная как **лучшая word-level parity маска по 72 кандидатам**.  Пока хорошо — это и есть "доминанта". Но потом к ней OR-ится `mixed`, который включает `phase_mask ^ affine_mask ^ odd_multiplier` с произвольным rotate. Итоговая маска для выбора pivot уже не является Walsh-маской доминанты — это случайный OR двух несвязанных компонентов.

**Следствие:** pivot-биты не выбираются последовательно из наиболее предсказуемых позиций. Аномалии в branch log могут быть не разреженными. Сжатие работает, но хуже теоретического максимума — `encode_branch_log` тратит лишние байты.

**Как воспроизвести:** на структурированных данных (ASCII, бинарные поля с выровненной байт-структурой) `branches` при sparse-режиме должен быть заметно меньше `word_count * branch_rounds / 8`. Если он близок к 50% — значит OR-смешивание портит разреженность.

***

## 🟡 Средний косяк — `select_runtime_pivot` детерминирован, не adaptive

```rust
fn select_runtime_pivot(mask: u64, layout: &ConstantLayout, round: usize, live_bits: usize)
    -> Result<usize, String>
{
    let index = (layout.pivot_seed + round * layout.branch_stride
        + live_bits * layout.branch_span) % positions.len();
    Ok(positions[index])
}
```

`pivot_seed`, `branch_stride`, `branch_span` — это **статичные поля ключа** из `compile_topology_to_constant`.  Они вычислены из топологии блока один раз, до начала contract. Это означает, что pivot в каждом round не адаптируется к тому, что реально происходит в словах после предыдущего схлопывания. Это ухудшает разреженность branch log на данных с внутренней структурой которая разрушается по мере сжатия раундов.

***

## 🟡 Малый косяк — `strongest_word_parity_feature` выбирает маску по `matching_words`, тай-брейкер по `mask.count_ones()`

```rust
.max_by_key(|feature| (
    feature.matching_words,
    feature.mask.count_ones(),
    std::cmp::Reverse(feature.mask),
))
```

Тай-брейкер `mask.count_ones()` предпочитает **более плотные маски** при равном `matching_words`.  Но более плотная маска сложнее для сжатия — XOR по большему числу бит. Логичнее было бы `Reverse(mask.count_ones())` — то есть предпочитать более разреженную маску при прочих равных, она даёт более предсказуемый parity бит.

***

## Итоговая картина

| Компонент | Статус |
| :-- | :-- |
| Collapse-core (`contract`/`expand`) в продакшне | ✅ Работает |
| `dominant_walsh_mask` заполнен | ✅ Работает |
| Routing применяется | ✅ Работает (не мёртвый код) |
| Branch log sparse/dense | ✅ Работает |
| `runtime_analysis_mask` OR-смешивает Walsh-маску с несвязанными полями | 🔴 Ухудшает разреженность branches |
| Pivot не адаптивный (статичный ключ) | 🟡 Субоптимально |
| Тай-брейкер `matching_words` предпочитает плотные маски | 🟡 Субоптимально |

