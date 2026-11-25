use memb_core::Note;
use memb_embed::{reciprocal_rank_fusion, QuantizedVec};
use memb_store::Store;

/// Hybrid retrieval combining lexical ranking + quantized vector ANN + RRF (k=60) + 1-hop graph expansion.
/// Gracefully degrades to pure lexical search when `query_vec` is None (e.g. llama-server unavailable).
pub fn hybrid_search(
    store: &Store,
    query: &str,
    query_vec: Option<&QuantizedVec>,
    hot_index: &[(u64, QuantizedVec)],
) -> Result<Vec<Note>, String> {
    let lexical_ids = store.search_lexical_ids(query, 10)?;
    let fused_seeds = if let Some(qv) = query_vec {
        let mut vec_scored: Vec<(f32, u64)> = hot_index
            .iter()
            .map(|(id, nv)| (qv.cosine(nv), *id))
            .filter(|(s, _)| *s > 0.25)
            .collect();
        vec_scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let vector_ids: Vec<u64> = vec_scored.into_iter().take(10).map(|(_, id)| id).collect();
        reciprocal_rank_fusion(&lexical_ids, &vector_ids, 5)
    } else {
        lexical_ids.into_iter().take(5).collect()
    };
    store.expand_one_hop(&fused_seeds, 8)
}
