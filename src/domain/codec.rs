// DIRECT BIFURCATION ENCODING
// The operator directly consumes and encodes Vector V by observing
// the intrinsic properties of the data at topological bifurcation points.
fn encode_bifurcation_trajectory(
    words: &[u64],
    blueprint: &OperatorBlueprint,
    steps: u8,
) -> (Vec<u64>, Vec<bool>) {
    let mut state = words.to_vec();
    let mut crumbs = Vec::new();
    
    // Wire topological parameters to deterministic structural levers
    let parity = (blueprint.shift_match as u64) | ((blueprint.dominant_index as u64) << 16);
    let m0 = blueprint.primary_shift as u32;
    let m1 = (blueprint.secondary_delta as u32).max(1);
    let mask = blueprint.popcnt_density;
    
    for _ in 0..steps {
        for (idx, word) in state.iter_mut().enumerate() {
            // Forward deterministic linear step
            *word ^= parity;
            *word = word.rotate_left(m0);
            
            // Critical point: Direct observation at the topological bifurcation
            if (idx & (mask as usize)) == (mask as usize) {
                // Observe the intrinsic property (MSB representing phase)
                let is_negative = (*word >> 63) == 1;
                crumbs.push(is_negative);
                
                // Normalize state to strictly reduce entropy
                if is_negative {
                    *word = !*word;
                }
            }
            
            // Complete the linear step
            *word ^= parity.rotate_right(m1);
        }
    }
    (state, crumbs)
}

// EXACT REVERSIBLE DECODING
// The operator perfectly restores the original data by injecting Vector V
// backwards at the exact topological bifurcation points.
fn decode_bifurcation_trajectory(
    terminals: &[u64],
    crumbs: &[bool],
    blueprint: &OperatorBlueprint,
    steps: u8,
) -> Result<Vec<u64>, String> {
    let mut state = terminals.to_vec();
    let mut crumb_idx = crumbs.len();
    
    let parity = (blueprint.shift_match as u64) | ((blueprint.dominant_index as u64) << 16);
    let m0 = blueprint.primary_shift as u32;
    let m1 = (blueprint.secondary_delta as u32).max(1);
    let mask = blueprint.popcnt_density;
    
    for _ in 0..steps {
        // Must iterate backwards over the spatial domain to correctly match the stack
        for (idx, word) in state.iter_mut().enumerate().rev() {
            *word ^= parity.rotate_right(m1);
            
            if (idx & (mask as usize)) == (mask as usize) {
                if crumb_idx == 0 {
                    return Err("bifurcation stream exhausted before completion".to_string());
                }
                crumb_idx -= 1;
                // Restore the observed intrinsic property
                let was_negative = crumbs[crumb_idx];
                if was_negative {
                    *word = !*word;
                }
            }
            
            *word = word.rotate_right(m0);
            *word ^= parity;
        }
    }
    
    if crumb_idx != 0 {
        return Err("bifurcation stream contains unused trailing crumbs".to_string());
    }
    
    Ok(state)
}

fn try_encode_operator_block(block: &[u8]) -> Result<Option<BlockEncoding>, String> {
    if block.len() < 64 || block.len() % 8 != 0 || !block.len().is_power_of_two() {
        return Ok(None);
    }
    
    let words = block
        .chunks_exact(8)
        .map(|chunk| u64::from_le_bytes(chunk.try_into().unwrap()))
        .collect::<Vec<_>>();
        
    let signature = analyze_topology(block)?;
    let base_key = compile_topology_to_key(&signature)?;
    let blueprint = base_key.operator_blueprint()?;
    
    let mut best: Option<BlockEncoding> = None;

    for steps in candidate_steps() {
        // Direct internal encoding instead of post-flight patching
        let (terminals, crumbs) = encode_bifurcation_trajectory(&words, &blueprint, steps);
        
        let unique_terminals = terminals
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
            
        if unique_terminals.is_empty() || unique_terminals.len() > 256 {
            continue;
        }
        
        let terminal_index_bits = bit_width_for_cardinality(unique_terminals.len());
        let index_by_terminal: BTreeMap<u64, u8> = unique_terminals
            .iter()
            .enumerate()
            .map(|(index, &terminal)| (terminal, index as u8))
            .collect();
            
        let terminal_indices = terminals
            .iter()
            .map(|t| index_by_terminal.get(t).copied().unwrap())
            .collect::<Vec<_>>();
        
        let key_bytes = base_key.serialize().to_vec();
        
        let candidate = BlockEncoding::Operator(OperatorBlock {
            original_len: block.len() as u32,
            key: key_bytes.clone(),
            terminals: unique_terminals,
            terminal_indices: pack_indices(&terminal_indices, terminal_index_bits),
            steps,
            breadcrumbs: pack_bools(&crumbs),
        });
        
        let overhead_bytes = encoded_size_of(&candidate)
            .checked_sub(key_bytes.len())
            .and_then(|value| value.checked_sub(extract_operator_block(&candidate).unwrap().breadcrumbs.len()))
            .and_then(|value| value.checked_sub(extract_operator_block(&candidate).unwrap().terminal_indices.len()))
            .ok_or_else(|| "operator block accounting underflow".to_string())?;
            
        let budget = BitBudget {
            source_bits: (block.len() * 8) as u64,
            key_bits: (key_bytes.len() * 8) as u64,
            crumb_bits: ((extract_operator_block(&candidate).unwrap().breadcrumbs.len()
                + extract_operator_block(&candidate).unwrap().terminal_indices.len())
                * 8) as u64,
            overhead_bits: (overhead_bytes * 8) as u64,
        };
        
        if CompressionPolicy::MVP.accepts(budget)?
            && best.as_ref().map(|current| encoded_size_of(&candidate) < encoded_size_of(current)).unwrap_or(true)
        {
            best = Some(candidate);
        }
    }
    
    Ok(best)
}

fn decode_operator_block(block: &OperatorBlock) -> Result<Vec<u8>, String> {
    if block.original_len as usize % 8 != 0 {
        return Err("operator block length must be divisible by 8".to_string());
    }
    
    let key = MagicKey::parse(&block.key)?;
    key.require_kind(MagicKeyKind::Operator)?;
    let blueprint = key.operator_blueprint()?;
    
    let steps = block.steps;
    if !(1..64).contains(&steps) {
        return Err("operator parity step count must be in 1..64".to_string());
    }
    
    let word_count = block.original_len as usize / 8;
    let terminal_indices = unpack_indices(
        &block.terminal_indices,
        bit_width_for_cardinality(block.terminals.len()),
        word_count,
    )?;
    
    let mut terminals = Vec::with_capacity(word_count);
    for index in terminal_indices {
        terminals.push(
            *block.terminals
            .get(index as usize)
            .ok_or_else(|| "operator terminal index is out of range".to_string())?
        );
    }
    
    // Calculate the exact amount of expected crumbs from structural invariants
    let mask = blueprint.popcnt_density;
    let bifurcation_points_per_round = (0..word_count)
        .filter(|idx| (idx & (mask as usize)) == (mask as usize))
        .count();
    let exact_crumb_count = bifurcation_points_per_round * (steps as usize);
    
    let crumbs = unpack_bools(&block.breadcrumbs, exact_crumb_count)?;
    
    let decoded_words = decode_bifurcation_trajectory(&terminals, &crumbs, &blueprint, steps)?;
    
    let mut out = Vec::with_capacity(block.original_len as usize);
    for word in decoded_words {
        out.extend_from_slice(&word.to_le_bytes());
    }
    
    Ok(out)
}