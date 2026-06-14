<img src="https://r2cdn.perplexity.ai/pplx-full-logo-primary-dark%402x.png" style="height:64px;margin-right:32px"/>

# подтяни сейчас изменения из мастера и сделай ревью. все ли там правильно? или есть какие-то ошибки? что нужно поправить?

Отлично, полную картину получил. Анализирую всё что пришло в мастер после нашего мержа.

***

## Состояние мастера

После нашего мержа (`65a28b`) в мастер прилетело ещё **4 коммита «Metagenerator updated»** от Ilya Rehemae.  В них три изменённых файла: `codec.rs` (+222/-71), `topology.rs` (+12/-3), и добавлен лог разговора с Gemini.

***

## Что было изменено — разбор по файлам

### `topology.rs` — `compile_strict_operator_steps`

Стало так :

```rust
if derivative_density >= 640 && dominance <= 704 { return Ok(1); }
if derivative_density >= 512 && shift_coherence <= 640 { return Ok(1); }
if derivative_density >= 384 { return Ok(2); }
let structured = dominance / 256 + shift_coherence / 384 + window_factor;
let noisy = derivative_density / 320;
Ok(structured.saturating_sub(noisy).clamp(1, 4) as u8)
```

**Что хорошо:** три явных ранних выхода для зашумлённых блоков — логично, экономит шаги на белом шуме.

**Проблемы:**

1. **Magic numbers вернулись.** `640`, `704`, `512`, `640`, `384`, `256`, `384`, `320` — всё без именованных констант и без объяснения. Мы специально убирали это в нашем фиксе — теперь всё вернулось.
2. **`clamp(1, 4)` вместо `clamp(1, 8)`** — максимум шагов снизили вдвое. Это намеренно или случайно? Нет комментария.
3. Логика `derivative_density >= 640 && dominance <= 704` противоречит нашим константам `DOMINANCE_THRESHOLD = 700` — теперь порог сдвинут на `704` без объяснения.

***

### `codec.rs` — крупные изменения

Добавлено много хорошего:

- **`StrictWindowCandidate` + `analyze_strict_window_candidate`** — отличная инкапсуляция, теперь кандидат несёт метрики, шаги и `floor_bits` вместе.
- **`floor_bits` как экономический фильтр** — перед выбором окна теперь проверяется `floor_bits * 10 < size * 8`, то есть минимальный размер закодированного блока должен быть < 100% сырых данных. Умно.
- **`operator_budget()`** — вынесен в отдельную функцию, убрано дублирование из `try_encode_operator_block` и `debug_strict_operator_block`. Хорошо.
- **`encode_operator_sparse_exceptions` — рефакторинг:**
    - Добавлен `position_mode` (0 = delta-кодирование, 1 = bitmap для ≤64 слов) — правильная оптимизация.
    - XOR-дельты (`value ^ dominant_word`) вместо raw u64 для исключений — правильно, снижает энтропию значений.
    - `delta_width` + `pack_operator_word_values` — bit-packing дельт по реальной ширине. Хорошо.

**Проблемы в `codec.rs`:**

1. **`OPERATOR_BLOCK_FIXED_OVERHEAD_BITS = 18 * 8`** — откуда `18`? Нет комментария, нет связи с реальной структурой формата. Это число надо либо вывести из существующих констант, либо хотя бы задокументировать.
2. **`STRICT_OPERATOR_MIN_TERMINAL_BITS = 64`** — минимум 64 бита для терминала — почему? Одно u64-слово? Нет объяснения.
3. **`floor_bits` фильтрация в `analyze_strict_pass_band`:** условие `(candidate.floor_bits * 10) < (candidate.size * 8) as u64` и аналогичное в `select_strict_window_plan` — **разные контексты одного предиката**, но вынесены в разные места без общей функции. Если логика изменится — придётся менять в двух местах.
4. **`decode_operator_sparse_exceptions` — новый минимум `payload.len() < 15`** (было 14) — добавился байт `position_mode`. Но при `exception_count == 0` и `delta_width == 0` проверка `cursor != payload.len()` срабатывает корректно? Нужен тест для пустых исключений.
5. **`decode_operator_position_bitmap`** — guard `bytes.len() != 8 || word_count > 64` корректен, но ошибка «bitmap length is not canonical» вводит в заблуждение — правильнее «bitmap mode requires exactly 8 bytes and word_count ≤ 64».
6. **`crumb_bits` в `operator_budget`** — теперь суммирует `breadcrumbs.len() + terminal_payload.len()`. Это изменение семантики по сравнению со старым кодом (где было только `breadcrumbs`). Если `terminal_payload` уже учитывается отдельно через `overhead_bytes`, будет двойной счёт. Нужно проверить.

***

### Главная проблема — «Парадокс фейковых крошек»

Gemini-лог в репо описывает фундаментальный архитектурный баг : `strict_word_forward` — полностью **биективна**, ни один бит не теряется. Вектор V (breadcrumbs) сохраняет данные, но декодер их **не читает** — он восстанавливает всё сам через инверсию. Это означает:

- Breadcrumbs — мёртвый груз, раздувающий архив
- Белый шум никогда не схлопнется в `SmallPalette` / `UniformWord`
- Это **не устранено** в новых коммитах — там только оптимизируется выбор окна и кодирование исключений

***

## Итог: что нужно исправить

| Приоритет | Файл | Проблема |
| :-- | :-- | :-- |
| 🔴 Критично | `operator.rs` | Биективный конвейер — breadcrumbs не участвуют в декодировании |
| 🟠 Важно | `topology.rs` | Magic numbers вернулись (640, 704, 512 и т.д.) без конст |

