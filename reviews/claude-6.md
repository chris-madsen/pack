<img src="https://r2cdn.perplexity.ai/pplx-full-logo-primary-dark%402x.png" style="height:64px;margin-right:32px"/>

# Да

Теперь у меня есть полная картина реального кода. Вот честная проверка плана.

***

## Что план описывает правильно

**Главный диагноз верен.** `try_encode_operator_block` действительно вызывает `apply_strict_operator_schedule` — биективную функцию — и записывает `breadcrumbs` в wire format, но `decode_operator_block` восстанавливает слова через `apply_strict_operator_inverse_step` математически, без чтения `breadcrumbs`. Breadcrumbs в декодере используются только как верификация, а не как вектор восстановления. Это подтверждено кодом.

**`contract_block`/`expand_block` изолированы за `#[cfg(test)]`** — верно. В `codec_tests.rs` они используются напрямую (`use crate::domain::kernel::operator::{contract_block, expand_block, materialize_runtime}`), но в production-пути (`try_encode_operator_block`, `decode_operator_block`) их нет.

***

## Где план был галлюцинацией — теперь подтверждено

**Пункт 3 плана: "`K` должен кодировать параметры collapse-pass, сейчас он кодирует биективную перестановку"** — это **неверно**. В реальном коде `K` — это `MagicKey` (64-битный SpectralKey или TrajectoryKey). Никаких `ConstantLayout`, `PackedConstantK`, `branch_rounds`, `pivot_seed` и т.д. в production-пути нет — они все под `#[cfg(test)]`. `compile_strict_operator_key` возвращает struct с `bit_len: u16` и `bytes: Vec<u8>` — просто сырые байты ключа для `apply_strict_operator_schedule`. Это совсем другое, чем то, из чего строится `RuntimePipeline` для `contract_block`.

Реальный шов: у `contract_block` принимает `RuntimePipeline`, который строится из `PackedConstantK` через `materialize_runtime`. В production-пути нет ни `PackedConstantK`, ни `materialize_runtime`. То есть, чтобы подключить `contract_block`, нужно не просто "переименовать вызов" — нужно либо сделать `compile_topology_to_constant` (которая есть в тестах) production-функцией, либо построить новый мост. **Этого в плане нет.**

**Пункт 3 плана: "убрать append-and-replay / opcode VM"** — галлюцинация подтверждена. В коде такого нет.

***

## Критические пробелы, которые план не покрывает

**1. Нет пути от topology → `RuntimePipeline` в production.** `compile_topology_to_constant` — под `#[cfg(test)]`. `materialize_runtime` — под `#[cfg(test)]`. Plan говорит "переключить вызов в `try_encode_operator_block`", но туда нельзя подключить `contract_block` без того, чтобы сначала вывести эти две функции из-под `#[cfg(test)]`.

**2. Wire format несовместим.** Сейчас `OperatorBlock` хранит `breadcrumbs: Vec<u8>` отдельно от `terminal_payload`. Если `contract_block` вернёт `(seed, branches)`, то `seed` → это новый `terminal_payload`, а `branches` → это новый `breadcrumbs`. Поле `steps` в `OperatorBlock` сейчас означает "шаги биективной функции", но после фикса должно означать "число branch rounds". Семантика поля меняется — это нужно явно прописать в плане, потому что `steps == 0` будет ошибкой, а `branch_rounds` из layout не то же самое.

**3. `operator_budget` в codec.rs будет сломан.** Сейчас `crumb_bits` = `breadcrumbs.len() * 8 + terminal_payload.len() * 8` (объединено в одну метрику). После замены смысл `terminal_payload` кардинально меняется (это уже не raw слова, а seed), и формула бюджета может давать неверный результат.

**4. `analyze_strict_window_candidate` считает `floor_bits` через `breadcrumb_bits = word_count * steps`** — по формуле для старого биективного пути. После замены формула должна быть `branch_bits = word_count * branch_rounds` из `ConstantLayout`. Это разные числа, и window selection выберет не тот размер блока.

***

## Итоговая таблица

| Утверждение плана | Статус после проверки кода |
| :-- | :-- |
| Текущий strict path — биекция, breadcrumbs не нужны для decode | ✅ Верно |
| `contract_block`/`expand_block` — за `#[cfg(test)]`, не используются в production | ✅ Верно |
| `K` нужно переписать под collapse | ❌ Галлюцинация — `K` это `MagicKey` (u64), не `ConstantLayout` |
| "Убрать append-and-replay / opcode VM" | ❌ Галлюцинация — этого нет в коде |
| Подключить `contract_block` — просто смена вызова в двух местах | ⚠️ Неполно — нужно вывести `compile_topology_to_constant` + `materialize_runtime` из `#[cfg(test)]` |
| `steps` в `OperatorBlock` становится `branch_rounds` | ❌ Не упомянуто — семантика поля меняется |
| `analyze_strict_window_candidate` floor_bits формула | ❌ Не упомянуто — нужно обновить под `branch_rounds` |
| `operator_budget` crumb_bits формула | ❌ Не упомянуто — может сломаться |

