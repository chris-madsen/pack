# Матрица контрактных unit-тестов ядра `pack`

**Статус:** 59 тестов, все проходят.  
**Команда:** `cargo test`

Тесты фиксируют семантику из `docs/TZ.md`, `docs/zip.md` и `docs/gemini-conversation-2026-06-12-20-22-05.md`.

## 1. Покрытие обязательного ядра

| Пункт | Контракт | Основные тесты |
|---|---|---|
| Формальная `B_STD` | Версионированная детерминированная база; уникальные коды; обязательные ARX/Walsh/phase операции; необратимые макросы разрешены только внутри Feistel/phase | `b_std_is_deterministic_and_contains_required_algebra`, `irreversible_macros_are_restricted_to_feistel_or_phase_context`, `unknown_base_version_is_rejected` |
| `K_ROOT -> GenerateBase(...)` | Файловая база детерминированно порождается из корня; одинаковый `K_ROOT` даёт одинаковую базу; разный `K_ROOT` меняет расписание | `k_root_generates_the_same_file_base_for_both_sides`, `changing_k_root_changes_generated_base_schedule` |
| Гибридный формат `K` | `KHeader + ordered segments + payloads`; канонический roundtrip; версия; отсутствие хвоста; лимит 512 бит | `hybrid_k_roundtrips_canonically_and_is_versioned`, `segment_order_is_semantic_and_noncanonical_order_is_rejected`, `k_over_512_bits_is_rejected`, `malformed_or_extended_k_is_rejected` |
| FWHT | Только степень двойки; сохранение энергии; повторное применение восстанавливает вход; известный DC-спектр | `fwht_is_an_involution_and_preserves_energy`, `constant_signal_has_only_dc_walsh_peak`, `fwht_rejects_non_power_of_two_lengths` |
| Walsh-пики | Выбор по модулю коэффициента; детерминированный tie-break по индексу | `walsh_peaks_use_absolute_strength_and_canonical_ties` |
| Битовые производные | Точная семантика `bit[i] XOR bit[(i+s) mod n]` | `derivative_is_xor_with_requested_circular_shift` |
| Shift-correlation | Считает совпадения с циклическим сдвигом; обнаруживает известный период | `shift_correlation_detects_periodic_topology` |
| Popcnt-profile | Точный popcnt каждого окна | `popcnt_profile_counts_each_window_exactly` |
| `Topology → K` | Чистая детерминированная компиляция; изменение топологии меняет `K`; в `K` входят shift scores, Walsh peaks, spectrum fold, derivative fold и popcnt fold; множитель нечётный | `topology_compiler_is_deterministic_and_uses_all_required_profiles`, `changing_block_topology_changes_compiled_k` |
| Feistel/ARX `F_K` | Явный обратный ход на множестве состояний; нечётный множитель; необратимый popcnt допустим только внутри Feistel | `feistel_arx_fk_roundtrips_many_states`, `popcnt_inside_round_does_not_break_feistel_invertibility`, `even_multiplier_is_rejected` |
| Фазовый `D_K` | Фазовая функция параметризуется ключом; повторное применение возвращает исходное состояние | `d_k_phase_reflection_is_an_involution` |
| Самообратный `U_K` | `F_K⁻¹ · H · D_K · H · F_K`; тот же оператор применяется дважды; разные `K` дают разные операторы | `u_k_is_self_inverse_for_multiple_keys_and_states`, `fk_index_mix_has_an_explicit_inverse`, `changing_k_changes_the_operator` |
| Траекторный `V` | Крошки записаны в порядке прямого хода; читаются назад; каждая крошка влияет на результат; скрытое состояние не требуется | `v_contains_exact_forward_parity_order_and_reconstructs_backwards`, `every_crumb_is_semantically_required_for_unique_reconstruction`, `decoder_requires_only_terminal_and_v_without_hidden_state` |
| Метрики `K/V/overhead` | Отдельный точный учёт; вычисление gain и crumb ratio | `metrics_account_for_k_v_and_overhead_separately` |
| Hard constraint | Строгое сжатие; минимум 10% gain для MVP; `K <= min(512, N/4)`; равенство размеров не принимается | `hard_constraint_rejects_non_compression_and_less_than_ten_percent_gain`, `hard_constraint_enforces_variable_k_upper_bound`, `exact_boundary_is_not_accepted_as_strict_compression` |
| Жадный экономический аудит | Новое правило в `K` принимается только если уменьшение `V` больше прироста `K` | `greedy_budget_accepts_only_profitable_rules` |
| Адаптивное спектральное окно | Границы окна выбираются послойно, сохраняют степени двойки и не теряют базовые MVP-якори | `adaptive_window_bounds_are_power_of_two_and_layer_local`, `default_adaptive_candidates_include_mvp_and_larger_blocks` |
| Анти-подмена `V` | Операторный путь не принимает полноразмерный residual как нормальную семантику `V` | `spectral_mode_rejects_full_block_residual_semantics_on_noise` |
| Качество на generator-shaped ensemble | Для данных, порождённых детерминированным генератором, реальный кодек обязан давать >=10x при lossless-восстановлении | `generator_shaped_block_achieves_at_least_ten_x_compression`, `generator_shaped_stress_ensemble_keeps_ten_x_and_roundtrip` |
| Trajectory block в реальном кодеке | `V` читается по траектории, заданной `K`, без внешнего `CRUMB_CFG`; декодер имеет gas limit; невыгодный trajectory-path падает в raw | `trajectory_block_roundtrips_through_real_archive`, `trajectory_decoder_requires_only_key_and_v_without_external_crumb_cfg`, `trajectory_decoder_rejects_gas_limit_exceeded`, `trajectory_mode_falls_back_to_raw_when_not_profitable` |
| Качество trajectory-path | trajectory-shaped блок обязан давать >=10x при lossless roundtrip | `trajectory_shaped_block_achieves_at_least_ten_x_compression` |

## 2. Regression-покрытие ранее найденных дефектов

| Найденный дефект | Тест |
|---|---|
| `4096 бит` были реализованы как `4096 байт` | `mvp_block_size_is_4096_bits_not_4096_bytes` |
| CLI сужал адаптивный набор размеров блока | `default_adaptive_candidates_include_mvp_and_larger_blocks` |
| Слой принимался без root-overhead | `rejects_layer_when_root_overhead_erases_local_gain`, `accepted_layers_reduce_complete_recursive_archive` |
| Для односимвольного алфавита писался лишний бит на символ | `singleton_alphabet_requires_no_breadcrumb_bits` |
| Неизвестная версия базы принималась | `rejects_unsupported_base_version` |
| Метаданные цепочки слоёв не проверялись | `rejects_tampered_layer_size_chain` |
| Хвостовые байты архива игнорировались | `rejects_trailing_bytes` |
| Raw `original_len` мог не совпадать с payload | `rejects_raw_block_with_mismatched_original_length` |
| Не была проверена базовая lossless-обратимость контейнера | `roundtrip_ascii_digits`, `roundtrip_binary_noise` |
| Повторные слои могли не выполняться | `uses_multiple_layers_when_beneficial` |

## 3. Граница утверждения

Эта матрица означает **полное покрытие зафиксированных контрактов перечисленных пунктов**, а не математическое доказательство корректности любой будущей реализации.

Если семантика в ТЗ меняется, сначала должен меняться контрактный тест, и только затем реализация. Реализация ядра считается допустимой, только если весь набор тестов остаётся зелёным.
