//! Memory controller - REST API for the unified DB-backed memory system
//!
//! Provides endpoints for browsing, searching, and viewing memories.

use actix_web::{web, HttpRequest, HttpResponse, Responder};
use serde::{Deserialize, Serialize};

use crate::AppState;

/// Validate session token from request
fn validate_session_from_request(
    state: &web::Data<AppState>,
    req: &HttpRequest,
) -> Result<(), HttpResponse> {
    let token = req
        .headers()
        .get("Authorization")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.trim_start_matches("Bearer ").to_string());

    let token = match token {
        Some(t) => t,
        None => {
            return Err(HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "No authorization token provided"
            })));
        }
    };

    match state.db.validate_session(&token) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(HttpResponse::Unauthorized().json(serde_json::json!({
            "error": "Invalid or expired session"
        }))),
        Err(e) => {
            log::error!("Session validation error: {}", e);
            Err(HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal server error"
            })))
        }
    }
}

// ============================================================================
// Response Types
// ============================================================================

#[derive(Debug, Serialize)]
struct MemoryEntry {
    /// Synthetic path for backward compatibility (e.g., "2024-01-15.md" or "MEMORY.md")
    path: String,
    /// Memory type: "daily_log" or "long_term"
    memory_type: String,
    /// Parsed date if this is a daily log
    date: Option<String>,
    /// Identity ID
    identity_id: Option<String>,
    /// Number of entries for this date/type
    entry_count: i64,
}

#[derive(Debug, Serialize)]
struct ListFilesResponse {
    success: bool,
    files: Vec<MemoryEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct MemoryItem {
    id: i64,
    content: String,
    memory_type: String,
    importance: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    identity_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    log_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_type: Option<String>,
    created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_subtype: Option<String>,
}

#[derive(Debug, Serialize)]
struct ReadMemoriesResponse {
    success: bool,
    memory_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    identity_id: Option<String>,
    memories: Vec<MemoryItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct SearchResult {
    memory_id: i64,
    content: String,
    memory_type: String,
    importance: i64,
    score: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    identity_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    log_date: Option<String>,
}

/// A memory surfaced via graph edge expansion (connected to an FTS hit)
#[derive(Debug, Serialize)]
struct GraphResult {
    memory_id: i64,
    content: String,
    memory_type: String,
    importance: i64,
    /// Cumulative edge strength (×100) connecting this to the FTS seed set
    graph_strength: i32,
}

#[derive(Debug, Serialize)]
struct SearchResponse {
    success: bool,
    query: String,
    results: Vec<SearchResult>,
    /// Memories connected to the FTS hits via graph edges
    #[serde(skip_serializing_if = "Vec::is_empty")]
    graph_results: Vec<GraphResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct MemoryStatsResponse {
    success: bool,
    total_memories: i64,
    daily_log_count: i64,
    long_term_count: i64,
    identity_count: i64,
    identities: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    earliest_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    latest_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct AppendResponse {
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    similar_memories: Option<Vec<crate::memory::ConsolidationHint>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct MemoryInfoResponse {
    success: bool,
    backend: String,
    total_memories: i64,
}

// ============================================================================
// Request Types
// ============================================================================

#[derive(Debug, Deserialize)]
struct SearchQuery {
    query: String,
    #[serde(default = "default_search_limit")]
    limit: i32,
    identity_id: Option<String>,
    agent_subtype: Option<String>,
}

fn default_search_limit() -> i32 {
    20
}

#[derive(Debug, Deserialize)]
struct DailyLogQuery {
    date: Option<String>,
    identity_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LongTermQuery {
    identity_id: Option<String>,
    #[serde(default = "default_long_term_limit")]
    limit: Option<i32>,
}

fn default_long_term_limit() -> Option<i32> {
    None
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    identity_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AppendBody {
    content: String,
    identity_id: Option<String>,
    agent_subtype: Option<String>,
}

// ============================================================================
// Handlers
// ============================================================================

/// GET /api/memory/files - List memory dates and types
async fn list_files(
    data: web::Data<AppState>,
    req: HttpRequest,
    query: web::Query<ListQuery>,
) -> impl Responder {
    if let Err(resp) = validate_session_from_request(&data, &req) {
        return resp;
    }

    let identity_id = query.identity_id.as_deref();
    let mut files: Vec<MemoryEntry> = Vec::new();

    // Add long_term entry with count
    let long_term_count = match data.db.get_long_term_memories(identity_id, 1000) {
        Ok(mems) => mems.len() as i64,
        Err(_) => 0,
    };
    if long_term_count > 0 {
        files.push(MemoryEntry {
            path: "MEMORY.md".to_string(),
            memory_type: "long_term".to_string(),
            date: None,
            identity_id: identity_id.map(|s| s.to_string()),
            entry_count: long_term_count,
        });
    }

    // Add daily_log entries for each date
    if let Ok(dates) = data.db.list_memory_dates(identity_id) {
        for date in dates {
            let count = match data.db.get_daily_log_memories(&date, identity_id, 1000) {
                Ok(mems) => mems.len() as i64,
                Err(_) => 0,
            };
            files.push(MemoryEntry {
                path: format!("{}.md", date),
                memory_type: "daily_log".to_string(),
                date: Some(date),
                identity_id: identity_id.map(|s| s.to_string()),
                entry_count: count,
            });
        }
    }

    HttpResponse::Ok().json(ListFilesResponse {
        success: true,
        files,
        error: None,
    })
}

/// GET /api/memory/search - Search memories with BM25 full-text search
async fn search(
    data: web::Data<AppState>,
    req: HttpRequest,
    query: web::Query<SearchQuery>,
) -> impl Responder {
    if let Err(resp) = validate_session_from_request(&data, &req) {
        return resp;
    }

    let limit = query.limit.clamp(1, 100);

    match data.db.search_memories_fts_user(&query.query, query.identity_id.as_deref(), limit) {
        Ok(results) => {
            let seed_ids: Vec<i64> = results.iter().map(|(m, _)| m.id).collect();

            let results: Vec<SearchResult> = results
                .into_iter()
                .map(|(mem, rank)| SearchResult {
                    memory_id: mem.id,
                    content: if mem.content.chars().count() > 300 {
                        let truncated: String = mem.content.chars().take(300).collect();
                        format!("{}...", truncated)
                    } else {
                        mem.content
                    },
                    memory_type: mem.memory_type,
                    importance: mem.importance,
                    score: -rank, // Negate BM25 (returns negative)
                    identity_id: mem.identity_id,
                    log_date: mem.log_date,
                })
                .collect();

            // Graph expansion: surface memories connected to FTS hits via edges
            let graph_limit = (limit / 2).max(3).min(10);
            let graph_results = match data.db.graph_expand_from_seeds(&seed_ids, graph_limit) {
                Ok(neighbors) => neighbors
                    .into_iter()
                    .filter_map(|(neighbor_id, strength)| {
                        let mem = data.db.get_memory(neighbor_id).ok()??;
                        // Respect identity filter
                        if let Some(ref id_filter) = query.identity_id {
                            if mem.identity_id.as_deref() != Some(id_filter.as_str()) {
                                return None;
                            }
                        }
                        Some(GraphResult {
                            memory_id: mem.id,
                            content: if mem.content.chars().count() > 200 {
                                let truncated: String = mem.content.chars().take(200).collect();
                                format!("{}...", truncated)
                            } else {
                                mem.content
                            },
                            memory_type: mem.memory_type,
                            importance: mem.importance,
                            graph_strength: strength,
                        })
                    })
                    .collect(),
                Err(_) => vec![],
            };

            HttpResponse::Ok().json(SearchResponse {
                success: true,
                query: query.query.clone(),
                results,
                graph_results,
                error: None,
            })
        }
        Err(e) => HttpResponse::InternalServerError().json(SearchResponse {
            success: false,
            query: query.query.clone(),
            results: vec![],
            graph_results: vec![],
            error: Some(format!("Search failed: {}", e)),
        }),
    }
}

/// Helper to convert MemoryRows to MemoryItems
fn rows_to_items(rows: Vec<crate::db::tables::memories::MemoryRow>) -> Vec<MemoryItem> {
    rows.into_iter()
        .map(|m| MemoryItem {
            id: m.id,
            content: m.content,
            memory_type: m.memory_type,
            importance: m.importance,
            identity_id: m.identity_id,
            log_date: m.log_date,
            source_type: m.source_type,
            created_at: m.created_at,
            agent_subtype: m.agent_subtype,
        })
        .collect()
}

/// GET /api/memory/daily - Get today's or a specific date's daily log
async fn get_daily_log(
    data: web::Data<AppState>,
    req: HttpRequest,
    query: web::Query<DailyLogQuery>,
) -> impl Responder {
    if let Err(resp) = validate_session_from_request(&data, &req) {
        return resp;
    }

    let identity_id = query.identity_id.as_deref();

    let (date_str, memories) = if let Some(date_str) = &query.date {
        // Validate date format
        if chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d").is_err() {
            return HttpResponse::BadRequest().json(ReadMemoriesResponse {
                success: false,
                memory_type: "daily_log".to_string(),
                date: None,
                identity_id: None,
                memories: vec![],
                error: Some("Invalid date format. Use YYYY-MM-DD".to_string()),
            });
        }
        match data.db.get_daily_log_memories(date_str, identity_id, 200) {
            Ok(mems) => (date_str.clone(), mems),
            Err(e) => {
                return HttpResponse::InternalServerError().json(ReadMemoriesResponse {
                    success: false,
                    memory_type: "daily_log".to_string(),
                    date: Some(date_str.clone()),
                    identity_id: identity_id.map(|s| s.to_string()),
                    memories: vec![],
                    error: Some(format!("Failed to read daily log: {}", e)),
                });
            }
        }
    } else {
        // Get today's log
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        match data.db.get_today_daily_log(identity_id, 200) {
            Ok(mems) => (today, mems),
            Err(e) => {
                let today = chrono::Local::now().format("%Y-%m-%d").to_string();
                return HttpResponse::Ok().json(ReadMemoriesResponse {
                    success: true,
                    memory_type: "daily_log".to_string(),
                    date: Some(today),
                    identity_id: identity_id.map(|s| s.to_string()),
                    memories: vec![],
                    error: Some(format!("No entries yet: {}", e)),
                });
            }
        }
    };

    HttpResponse::Ok().json(ReadMemoriesResponse {
        success: true,
        memory_type: "daily_log".to_string(),
        date: Some(date_str),
        identity_id: identity_id.map(|s| s.to_string()),
        memories: rows_to_items(memories),
        error: None,
    })
}

/// GET /api/memory/long-term - Get long-term memories
async fn get_long_term(
    data: web::Data<AppState>,
    req: HttpRequest,
    query: web::Query<LongTermQuery>,
) -> impl Responder {
    if let Err(resp) = validate_session_from_request(&data, &req) {
        return resp;
    }

    let identity_id = query.identity_id.as_deref();
    let limit = query.limit.unwrap_or(100);

    match data.db.get_long_term_memories(identity_id, limit) {
        Ok(mems) => HttpResponse::Ok().json(ReadMemoriesResponse {
            success: true,
            memory_type: "long_term".to_string(),
            date: None,
            identity_id: identity_id.map(|s| s.to_string()),
            memories: rows_to_items(mems),
            error: None,
        }),
        Err(e) => HttpResponse::Ok().json(ReadMemoriesResponse {
            success: true,
            memory_type: "long_term".to_string(),
            date: None,
            identity_id: identity_id.map(|s| s.to_string()),
            memories: vec![],
            error: Some(format!("No long-term memories yet: {}", e)),
        }),
    }
}

/// POST /api/memory/daily - Append to today's daily log
async fn append_daily_log(
    data: web::Data<AppState>,
    req: HttpRequest,
    body: web::Json<AppendBody>,
) -> impl Responder {
    if let Err(resp) = validate_session_from_request(&data, &req) {
        return resp;
    }

    let identity_id = body.identity_id.as_deref();
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();

    match data.db.insert_memory(
        "daily_log",
        &body.content,
        None, None, 5, identity_id, None, None, None,
        Some("api"), Some(&today), body.agent_subtype.as_deref(),
    ) {
        Ok(id) => {
            // Check for consolidation hints if hybrid search is available
            let hints = if let Some(engine) = &data.hybrid_search {
                let h = engine.find_consolidation_hints(&body.content, 3).await;
                // Filter out the just-inserted memory from hints
                let h: Vec<_> = h.into_iter().filter(|hint| hint.memory_id != id).collect();
                if h.is_empty() { None } else { Some(h) }
            } else {
                None
            };

            HttpResponse::Ok().json(AppendResponse {
                success: true,
                message: Some("Added to daily log".to_string()),
                memory_id: Some(id),
                similar_memories: hints,
                error: None,
            })
        }
        Err(e) => HttpResponse::InternalServerError().json(AppendResponse {
            success: false,
            message: None,
            memory_id: None,
            similar_memories: None,
            error: Some(format!("Failed to append: {}", e)),
        }),
    }
}

/// POST /api/memory/long-term - Append to long-term memory
async fn append_long_term(
    data: web::Data<AppState>,
    req: HttpRequest,
    body: web::Json<AppendBody>,
) -> impl Responder {
    if let Err(resp) = validate_session_from_request(&data, &req) {
        return resp;
    }

    let identity_id = body.identity_id.as_deref();

    match data.db.insert_memory(
        "long_term",
        &body.content,
        None, None, 5, identity_id, None, None, None,
        Some("api"), None, body.agent_subtype.as_deref(),
    ) {
        Ok(id) => {
            // Check for consolidation hints if hybrid search is available
            let hints = if let Some(engine) = &data.hybrid_search {
                let h = engine.find_consolidation_hints(&body.content, 3).await;
                let h: Vec<_> = h.into_iter().filter(|hint| hint.memory_id != id).collect();
                if h.is_empty() { None } else { Some(h) }
            } else {
                None
            };

            HttpResponse::Ok().json(AppendResponse {
                success: true,
                message: Some("Added to long-term memory".to_string()),
                memory_id: Some(id),
                similar_memories: hints,
                error: None,
            })
        }
        Err(e) => HttpResponse::InternalServerError().json(AppendResponse {
            success: false,
            message: None,
            memory_id: None,
            similar_memories: None,
            error: Some(format!("Failed to append: {}", e)),
        }),
    }
}

/// GET /api/memory/stats - Get memory statistics
async fn get_stats(data: web::Data<AppState>, req: HttpRequest) -> impl Responder {
    if let Err(resp) = validate_session_from_request(&data, &req) {
        return resp;
    }

    match data.db.get_memory_stats() {
        Ok(stats) => HttpResponse::Ok().json(MemoryStatsResponse {
            success: true,
            total_memories: stats.total_memories,
            daily_log_count: stats.daily_log_count,
            long_term_count: stats.long_term_count,
            identity_count: stats.identity_count,
            identities: stats.identities,
            earliest_date: stats.earliest_date,
            latest_date: stats.latest_date,
            error: None,
        }),
        Err(e) => HttpResponse::InternalServerError().json(MemoryStatsResponse {
            success: false,
            total_memories: 0,
            daily_log_count: 0,
            long_term_count: 0,
            identity_count: 0,
            identities: vec![],
            earliest_date: None,
            latest_date: None,
            error: Some(format!("Failed to get stats: {}", e)),
        }),
    }
}

/// POST /api/memory/reindex - Rebuild the FTS5 index from the memories table
async fn reindex(data: web::Data<AppState>, req: HttpRequest) -> impl Responder {
    if let Err(resp) = validate_session_from_request(&data, &req) {
        return resp;
    }

    match data.db.rebuild_fts_index() {
        Ok(()) => {
            let count = data.db.count_memories().unwrap_or(0);
            HttpResponse::Ok().json(AppendResponse {
                success: true,
                message: Some(format!("FTS index rebuilt from {} memories.", count)),
                memory_id: None,
                similar_memories: None,
                error: None,
            })
        }
        Err(e) => HttpResponse::InternalServerError().json(AppendResponse {
            success: false,
            message: None,
            memory_id: None,
            similar_memories: None,
            error: Some(format!("Failed to rebuild FTS index: {}", e)),
        }),
    }
}

/// GET /api/memory/info - Get memory system info
async fn memory_info(data: web::Data<AppState>, req: HttpRequest) -> impl Responder {
    if let Err(resp) = validate_session_from_request(&data, &req) {
        return resp;
    }

    let total = data.db.count_memories().unwrap_or(0);

    HttpResponse::Ok().json(MemoryInfoResponse {
        success: true,
        backend: "sqlite".to_string(),
        total_memories: total,
    })
}

// ============================================================================
// Graph & Association Types
// ============================================================================

#[derive(Debug, Serialize)]
struct GraphNode {
    id: i64,
    content: String,
    memory_type: String,
    importance: i32,
}

#[derive(Debug, Serialize)]
struct GraphEdge {
    source: i64,
    target: i64,
    association_type: String,
    strength: f64,
}

#[derive(Debug, Serialize)]
struct GraphResponse {
    success: bool,
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
    max_graph_render_nodes: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct AssociationResponse {
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    associations: Option<Vec<AssociationItem>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    created: Option<AssociationItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deleted: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct AssociationItem {
    id: i64,
    source_memory_id: i64,
    target_memory_id: i64,
    association_type: String,
    strength: f64,
    created_at: String,
}

#[derive(Debug, Deserialize)]
struct CreateAssociationBody {
    source_memory_id: i64,
    target_memory_id: i64,
    #[serde(default = "default_association_type")]
    association_type: String,
    #[serde(default = "default_strength")]
    strength: f64,
}

fn default_association_type() -> String {
    "related".to_string()
}

fn default_strength() -> f64 {
    0.5
}

#[derive(Debug, Deserialize)]
struct AssociationQuery {
    memory_id: i64,
}

#[derive(Debug, Deserialize)]
struct DeleteAssociationPath {
    id: i64,
}

#[derive(Debug, Serialize)]
struct HybridSearchResponse {
    success: bool,
    query: String,
    mode: String,
    results: Vec<HybridSearchItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct HybridSearchItem {
    memory_id: i64,
    content: String,
    memory_type: String,
    importance: i32,
    rrf_score: f64,
    fts_rank: Option<f64>,
    vector_similarity: Option<f32>,
    association_count: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct HybridSearchQuery {
    query: String,
    #[serde(default = "default_search_limit")]
    limit: i32,
    agent_subtype: Option<String>,
}

#[derive(Debug, Serialize)]
struct EmbeddingStatsResponse {
    success: bool,
    total_memories: i64,
    memories_with_embeddings: i64,
    memories_without_embeddings: i64,
    coverage_percent: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct BackfillResponse {
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

// ============================================================================
// Graph & Association Handlers
// ============================================================================

/// GET /api/memory/graph - Get memory graph data (nodes + edges)
async fn get_graph(data: web::Data<AppState>, req: HttpRequest) -> impl Responder {
    if let Err(resp) = validate_session_from_request(&data, &req) {
        return resp;
    }

    let conn = data.db.conn();

    // Get all memories as nodes
    let nodes: Vec<GraphNode> = {
        let mut stmt = match conn.prepare(
            "SELECT id, content, memory_type, importance FROM memories ORDER BY id",
        ) {
            Ok(s) => s,
            Err(e) => {
                return HttpResponse::InternalServerError().json(GraphResponse {
                    success: false,
                    nodes: vec![],
                    edges: vec![],
                    max_graph_render_nodes: 0,
                    error: Some(format!("Failed to query memories: {}", e)),
                });
            }
        };

        match stmt.query_map([], |row| {
            Ok(GraphNode {
                id: row.get(0)?,
                content: {
                    let c: String = row.get(1)?;
                    if c.chars().count() > 200 {
                        let truncated: String = c.chars().take(200).collect();
                        format!("{}...", truncated)
                    } else {
                        c
                    }
                },
                memory_type: row.get(2)?,
                importance: row.get::<_, f64>(3)?.round() as i32,
            })
        }) {
            Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
            Err(e) => {
                return HttpResponse::InternalServerError().json(GraphResponse {
                    success: false,
                    nodes: vec![],
                    edges: vec![],
                    max_graph_render_nodes: 0,
                    error: Some(format!("Failed to query memories: {}", e)),
                });
            }
        }
    };

    // Get all associations as edges
    let edges: Vec<GraphEdge> = {
        let mut stmt = match conn.prepare(
            "SELECT source_memory_id, target_memory_id, association_type, strength FROM memory_associations",
        ) {
            Ok(s) => s,
            Err(e) => {
                return HttpResponse::InternalServerError().json(GraphResponse {
                    success: false,
                    nodes: vec![],
                    edges: vec![],
                    max_graph_render_nodes: 0,
                    error: Some(format!("Failed to query associations: {}", e)),
                });
            }
        };

        match stmt.query_map([], |row| {
            Ok(GraphEdge {
                source: row.get(0)?,
                target: row.get(1)?,
                association_type: row.get(2)?,
                strength: row.get(3)?,
            })
        }) {
            Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
            Err(e) => {
                return HttpResponse::InternalServerError().json(GraphResponse {
                    success: false,
                    nodes: vec![],
                    edges: vec![],
                    max_graph_render_nodes: 0,
                    error: Some(format!("Failed to query associations: {}", e)),
                });
            }
        }
    };

    let bot_cfg = crate::models::BotConfig::load();

    HttpResponse::Ok().json(GraphResponse {
        success: true,
        nodes,
        edges,
        max_graph_render_nodes: bot_cfg.max_graph_render_nodes,
        error: None,
    })
}

/// POST /api/memory/associations - Create a new association
async fn create_association(
    data: web::Data<AppState>,
    req: HttpRequest,
    body: web::Json<CreateAssociationBody>,
) -> impl Responder {
    if let Err(resp) = validate_session_from_request(&data, &req) {
        return resp;
    }

    let strength = body.strength.clamp(0.0, 1.0);

    match data.db.create_memory_association(
        body.source_memory_id,
        body.target_memory_id,
        &body.association_type,
        strength,
        None,
    ) {
        Ok(id) => HttpResponse::Ok().json(AssociationResponse {
            success: true,
            associations: None,
            created: Some(AssociationItem {
                id,
                source_memory_id: body.source_memory_id,
                target_memory_id: body.target_memory_id,
                association_type: body.association_type.clone(),
                strength,
                created_at: chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            }),
            deleted: None,
            error: None,
        }),
        Err(e) => HttpResponse::InternalServerError().json(AssociationResponse {
            success: false,
            associations: None,
            created: None,
            deleted: None,
            error: Some(format!("Failed to create association: {}", e)),
        }),
    }
}

/// GET /api/memory/associations - List associations for a memory
async fn list_associations(
    data: web::Data<AppState>,
    req: HttpRequest,
    query: web::Query<AssociationQuery>,
) -> impl Responder {
    if let Err(resp) = validate_session_from_request(&data, &req) {
        return resp;
    }

    match data.db.get_memory_associations(query.memory_id) {
        Ok(associations) => {
            let items: Vec<AssociationItem> = associations
                .into_iter()
                .map(|a| AssociationItem {
                    id: a.id,
                    source_memory_id: a.source_memory_id,
                    target_memory_id: a.target_memory_id,
                    association_type: a.association_type,
                    strength: a.strength as f64,
                    created_at: a.created_at,
                })
                .collect();

            HttpResponse::Ok().json(AssociationResponse {
                success: true,
                associations: Some(items),
                created: None,
                deleted: None,
                error: None,
            })
        }
        Err(e) => HttpResponse::InternalServerError().json(AssociationResponse {
            success: false,
            associations: None,
            created: None,
            deleted: None,
            error: Some(format!("Failed to list associations: {}", e)),
        }),
    }
}

/// DELETE /api/memory/associations/{id} - Delete an association
async fn delete_association(
    data: web::Data<AppState>,
    req: HttpRequest,
    path: web::Path<i64>,
) -> impl Responder {
    if let Err(resp) = validate_session_from_request(&data, &req) {
        return resp;
    }

    let association_id = path.into_inner();

    match data.db.delete_memory_association(association_id) {
        Ok(deleted) => HttpResponse::Ok().json(AssociationResponse {
            success: true,
            associations: None,
            created: None,
            deleted: Some(deleted),
            error: None,
        }),
        Err(e) => HttpResponse::InternalServerError().json(AssociationResponse {
            success: false,
            associations: None,
            created: None,
            deleted: None,
            error: Some(format!("Failed to delete association: {}", e)),
        }),
    }
}

/// GET /api/memory/hybrid-search - Combined FTS + vector + graph search
async fn hybrid_search(
    data: web::Data<AppState>,
    req: HttpRequest,
    query: web::Query<HybridSearchQuery>,
) -> impl Responder {
    if let Err(resp) = validate_session_from_request(&data, &req) {
        return resp;
    }

    let engine = match &data.hybrid_search {
        Some(engine) => engine,
        None => {
            return HttpResponse::ServiceUnavailable().json(HybridSearchResponse {
                success: false,
                query: query.query.clone(),
                mode: "hybrid".to_string(),
                results: vec![],
                error: Some("Hybrid search engine not initialized".to_string()),
            });
        }
    };

    let limit = query.limit.clamp(1, 50) as usize;

    match engine.search(&query.query, limit, query.agent_subtype.as_deref()).await {
        Ok(results) => {
            let items: Vec<HybridSearchItem> = results
                .into_iter()
                .map(|r| HybridSearchItem {
                    memory_id: r.memory_id,
                    content: r.content,
                    memory_type: r.memory_type,
                    importance: r.importance,
                    rrf_score: r.rrf_score,
                    fts_rank: r.fts_rank,
                    vector_similarity: r.vector_similarity,
                    association_count: r.association_count,
                })
                .collect();

            HttpResponse::Ok().json(HybridSearchResponse {
                success: true,
                query: query.query.clone(),
                mode: "hybrid".to_string(),
                results: items,
                error: None,
            })
        }
        Err(e) => HttpResponse::InternalServerError().json(HybridSearchResponse {
            success: false,
            query: query.query.clone(),
            mode: "hybrid".to_string(),
            results: vec![],
            error: Some(format!("Hybrid search failed: {}", e)),
        }),
    }
}

/// GET /api/memory/embeddings/stats - Get embedding statistics
async fn embedding_stats(data: web::Data<AppState>, req: HttpRequest) -> impl Responder {
    if let Err(resp) = validate_session_from_request(&data, &req) {
        return resp;
    }

    let conn = data.db.conn();

    let total_memories: i64 = conn
        .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
        .unwrap_or(0);

    let memories_with_embeddings: i64 = conn
        .query_row("SELECT COUNT(*) FROM memory_embeddings", [], |row| row.get(0))
        .unwrap_or(0);

    let memories_without = total_memories - memories_with_embeddings;
    let coverage = if total_memories > 0 {
        (memories_with_embeddings as f64 / total_memories as f64) * 100.0
    } else {
        0.0
    };

    HttpResponse::Ok().json(EmbeddingStatsResponse {
        success: true,
        total_memories,
        memories_with_embeddings,
        memories_without_embeddings: memories_without,
        coverage_percent: coverage,
        error: None,
    })
}

/// POST /api/memory/embeddings/backfill - Trigger embedding backfill
async fn backfill_embeddings(data: web::Data<AppState>, req: HttpRequest) -> impl Responder {
    if let Err(resp) = validate_session_from_request(&data, &req) {
        return resp;
    }

    let engine = match &data.hybrid_search {
        Some(engine) => engine,
        None => {
            return HttpResponse::ServiceUnavailable().json(BackfillResponse {
                success: false,
                message: None,
                error: Some("Hybrid search engine not initialized. Embedding backfill requires an embedding provider.".to_string()),
            });
        }
    };

    // Check if a backfill is already running
    if engine.is_backfill_running() {
        return HttpResponse::Conflict().json(BackfillResponse {
            success: false,
            message: None,
            error: Some("A backfill is already running. Please wait for it to complete.".to_string()),
        });
    }

    // Run backfill in background
    let engine = engine.clone();
    tokio::spawn(async move {
        match engine.backfill_embeddings().await {
            Ok(count) => log::info!("[EMBEDDINGS] Backfill complete: {} embeddings generated", count),
            Err(e) => log::error!("[EMBEDDINGS] Backfill failed: {}", e),
        }
    });

    HttpResponse::Ok().json(BackfillResponse {
        success: true,
        message: Some("Embedding backfill started in background".to_string()),
        error: None,
    })
}

/// POST /api/memory/associations/rebuild - Trigger association discovery pass
async fn rebuild_associations(data: web::Data<AppState>, req: HttpRequest) -> impl Responder {
    if let Err(resp) = validate_session_from_request(&data, &req) {
        return resp;
    }

    let engine = match &data.hybrid_search {
        Some(engine) => engine,
        None => {
            return HttpResponse::ServiceUnavailable().json(BackfillResponse {
                success: false,
                message: None,
                error: Some("Hybrid search engine not initialized. Association rebuild requires an embedding provider.".to_string()),
            });
        }
    };

    let db = data.db.clone();
    let db2 = data.db.clone();
    let embedding_generator = engine.embedding_generator().clone();
    let config = crate::memory::association_loop::AssociationLoopConfig::default();

    tokio::spawn(async move {
        // Step 0: Backfill missing entity_name / category metadata from content
        match crate::memory::association_loop::backfill_memory_metadata(&db2) {
            Ok(count) => {
                if count > 0 {
                    log::info!("[ASSOCIATIONS] Backfilled metadata for {} memories", count);
                }
            }
            Err(e) => log::warn!("[ASSOCIATIONS] Metadata backfill failed: {}", e),
        }

        // Step 1: Reclassify existing "related" associations using metadata heuristics
        match crate::memory::association_loop::reclassify_existing_associations(&db2) {
            Ok(count) => {
                if count > 0 {
                    log::info!("[ASSOCIATIONS] Reclassified {} existing associations", count);
                }
            }
            Err(e) => log::error!("[ASSOCIATIONS] Reclassification failed: {}", e),
        }

        // Step 2: Discover new associations with proper type classification
        match crate::memory::association_loop::run_association_pass(&db, &embedding_generator, &config).await {
            Ok(()) => log::info!("[ASSOCIATIONS] Rebuild pass complete"),
            Err(e) => log::error!("[ASSOCIATIONS] Rebuild pass failed: {}", e),
        }
    });

    HttpResponse::Ok().json(BackfillResponse {
        success: true,
        message: Some("Association rebuild started in background (includes reclassification of existing associations)".to_string()),
        error: None,
    })
}

/// DELETE /api/memory/all - Delete all memories
async fn delete_all_memories(
    data: web::Data<AppState>,
    req: HttpRequest,
    body: web::Json<serde_json::Value>,
) -> impl Responder {
    if let Err(resp) = validate_session_from_request(&data, &req) {
        return resp;
    }

    // Require explicit confirmation
    if body.get("confirm").and_then(|v| v.as_bool()) != Some(true) {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "success": false,
            "error": "Must set confirm: true to delete all memories"
        }));
    }

    let conn = data.db.conn();

    // Delete related tables first (embeddings + associations cascade via FK,
    // but be explicit for FTS which uses triggers)
    let deleted_count = match conn.execute("DELETE FROM memories", []) {
        Ok(n) => n,
        Err(e) => {
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "success": false,
                "error": format!("Failed to delete memories: {}", e)
            }));
        }
    };

    // Reindex notes store
    if let Some(store) = data.dispatcher.notes_store() {
        let _ = store.reindex();
    }

    HttpResponse::Ok().json(serde_json::json!({
        "success": true,
        "deleted_count": deleted_count
    }))
}

// ============================================================================
// Merge, Export & Import Types
// ============================================================================

#[derive(Debug, Deserialize)]
struct MergeBody {
    memory_id_a: i64,
    memory_id_b: i64,
    #[serde(default = "default_merge_strategy")]
    strategy: String,
    custom_content: Option<String>,
}

fn default_merge_strategy() -> String {
    "append".to_string()
}

#[derive(Debug, Serialize)]
struct MergeResponse {
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    new_memory_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    superseded_ids: Option<Vec<i64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct MemoryExportData {
    version: u32,
    exported_at: String,
    total_memories: usize,
    memories: Vec<MemoryExportEntry>,
    #[serde(default)]
    associations: Vec<AssociationExportEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct MemoryExportEntry {
    original_id: i64,
    memory_type: String,
    content: String,
    category: Option<String>,
    tags: Option<String>,
    importance: i64,
    identity_id: Option<String>,
    entity_type: Option<String>,
    entity_name: Option<String>,
    source_type: Option<String>,
    log_date: Option<String>,
    created_at: String,
    #[serde(default)]
    agent_subtype: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct AssociationExportEntry {
    source_original_id: i64,
    target_original_id: i64,
    association_type: String,
    strength: f64,
    metadata: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExportQuery {
    memory_type: Option<String>,
    identity_id: Option<String>,
    date_from: Option<String>,
    date_to: Option<String>,
    #[serde(default = "default_include_associations")]
    include_associations: bool,
}

fn default_include_associations() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct ImportBody {
    #[serde(default = "default_import_strategy")]
    strategy: String,
    data: MemoryExportData,
}

fn default_import_strategy() -> String {
    "append".to_string()
}

#[derive(Debug, Serialize)]
struct ImportResponse {
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    imported_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    skipped_duplicates: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    associations_created: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

// ============================================================================
// Merge, Export & Import Handlers
// ============================================================================

/// POST /api/memory/merge - Merge two memories
async fn merge_memories(
    data: web::Data<AppState>,
    req: HttpRequest,
    body: web::Json<MergeBody>,
) -> impl Responder {
    if let Err(resp) = validate_session_from_request(&data, &req) {
        return resp;
    }

    use crate::db::tables::memories::MergeStrategy;

    let strategy = match body.strategy.as_str() {
        "append" => MergeStrategy::Append,
        "replace_with_newer" => MergeStrategy::ReplaceWithNewer,
        "custom" => {
            let content = match &body.custom_content {
                Some(c) if !c.is_empty() => c.clone(),
                _ => return HttpResponse::BadRequest().json(MergeResponse {
                    success: false,
                    new_memory_id: None,
                    superseded_ids: None,
                    error: Some("custom_content is required when strategy is \"custom\"".to_string()),
                }),
            };
            MergeStrategy::Custom(content)
        }
        _ => return HttpResponse::BadRequest().json(MergeResponse {
            success: false,
            new_memory_id: None,
            superseded_ids: None,
            error: Some(format!("Unknown strategy: \"{}\". Use append, replace_with_newer, or custom.", body.strategy)),
        }),
    };

    match data.db.merge_memories(body.memory_id_a, body.memory_id_b, &strategy) {
        Ok(new_id) => HttpResponse::Ok().json(MergeResponse {
            success: true,
            new_memory_id: Some(new_id),
            superseded_ids: Some(vec![body.memory_id_a, body.memory_id_b]),
            error: None,
        }),
        Err(e) => HttpResponse::InternalServerError().json(MergeResponse {
            success: false,
            new_memory_id: None,
            superseded_ids: None,
            error: Some(format!("Failed to merge: {}", e)),
        }),
    }
}

/// GET /api/memory/export - Export memories as JSON
async fn export_memories(
    data: web::Data<AppState>,
    req: HttpRequest,
    query: web::Query<ExportQuery>,
) -> impl Responder {
    if let Err(resp) = validate_session_from_request(&data, &req) {
        return resp;
    }

    let memories = match data.db.list_memories_filtered(
        query.memory_type.as_deref(),
        query.identity_id.as_deref(),
        query.date_from.as_deref(),
        query.date_to.as_deref(),
    ) {
        Ok(m) => m,
        Err(e) => {
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "success": false,
                "error": format!("Failed to export memories: {}", e)
            }));
        }
    };

    let memory_ids: Vec<i64> = memories.iter().map(|m| m.id).collect();

    let associations = if query.include_associations && !memory_ids.is_empty() {
        match data.db.list_associations_for_memories(&memory_ids) {
            Ok(assocs) => assocs
                .into_iter()
                .map(|a| AssociationExportEntry {
                    source_original_id: a.source_memory_id,
                    target_original_id: a.target_memory_id,
                    association_type: a.association_type,
                    strength: a.strength,
                    metadata: a.metadata,
                })
                .collect(),
            Err(e) => {
                log::warn!("Failed to export associations: {}", e);
                vec![]
            }
        }
    } else {
        vec![]
    };

    let export_entries: Vec<MemoryExportEntry> = memories
        .into_iter()
        .map(|m| MemoryExportEntry {
            original_id: m.id,
            memory_type: m.memory_type,
            content: m.content,
            category: m.category,
            tags: m.tags,
            importance: m.importance,
            identity_id: m.identity_id,
            entity_type: m.entity_type,
            entity_name: m.entity_name,
            source_type: m.source_type,
            log_date: m.log_date,
            created_at: m.created_at,
            agent_subtype: m.agent_subtype,
        })
        .collect();

    let export = MemoryExportData {
        version: 1,
        exported_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        total_memories: export_entries.len(),
        memories: export_entries,
        associations,
    };

    HttpResponse::Ok()
        .insert_header(("Content-Disposition", "attachment; filename=\"memories_export.json\""))
        .json(export)
}

/// POST /api/memory/import - Import memories from JSON
async fn import_memories(
    data: web::Data<AppState>,
    req: HttpRequest,
    body: web::Json<ImportBody>,
) -> impl Responder {
    if let Err(resp) = validate_session_from_request(&data, &req) {
        return resp;
    }

    let strategy = body.strategy.as_str();

    // Replace strategy: clear everything first
    if strategy == "replace" {
        if let Err(e) = data.db.clear_memories_for_restore() {
            return HttpResponse::InternalServerError().json(ImportResponse {
                success: false,
                imported_count: None,
                skipped_duplicates: None,
                associations_created: None,
                error: Some(format!("Failed to clear memories for replace: {}", e)),
            });
        }
    }

    let mut imported = 0usize;
    let mut skipped = 0usize;
    let mut id_mapping: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();

    for entry in &body.data.memories {
        // Merge strategy: check for duplicates via FTS
        if strategy == "merge" {
            if let Ok(similar) = data.db.find_similar_memories_fts(
                &entry.content,
                Some(&entry.memory_type),
                entry.identity_id.as_deref(),
                3,
            ) {
                // BM25 returns negative scores; strong match = more negative (< -5)
                if similar.iter().any(|(_, rank)| *rank < -5.0) {
                    skipped += 1;
                    // Map to the best match so associations can still reference it
                    if let Some((best, _)) = similar.first() {
                        id_mapping.insert(entry.original_id, best.id);
                    }
                    continue;
                }
            }
        }

        match data.db.insert_memory(
            &entry.memory_type,
            &entry.content,
            entry.category.as_deref(),
            entry.tags.as_deref(),
            entry.importance,
            entry.identity_id.as_deref(),
            None,
            entry.entity_type.as_deref(),
            entry.entity_name.as_deref(),
            entry.source_type.as_deref(),
            entry.log_date.as_deref(),
            entry.agent_subtype.as_deref(),
        ) {
            Ok(new_id) => {
                id_mapping.insert(entry.original_id, new_id);
                imported += 1;
            }
            Err(e) => {
                log::warn!("Failed to import memory {}: {}", entry.original_id, e);
            }
        }
    }

    // Recreate associations using the old->new ID mapping
    let mut associations_created = 0usize;
    for assoc in &body.data.associations {
        let new_source = id_mapping.get(&assoc.source_original_id);
        let new_target = id_mapping.get(&assoc.target_original_id);

        if let (Some(&src), Some(&tgt)) = (new_source, new_target) {
            if src == tgt {
                continue; // skip self-references
            }
            match data.db.create_memory_association(
                src,
                tgt,
                &assoc.association_type,
                assoc.strength,
                assoc.metadata.as_deref(),
            ) {
                Ok(_) => associations_created += 1,
                Err(e) => log::warn!(
                    "Failed to import association {}->{}: {}",
                    assoc.source_original_id,
                    assoc.target_original_id,
                    e
                ),
            }
        }
    }

    HttpResponse::Ok().json(ImportResponse {
        success: true,
        imported_count: Some(imported),
        skipped_duplicates: Some(skipped),
        associations_created: Some(associations_created),
        error: None,
    })
}

/// Configure memory routes
pub fn config(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/api/memory")
            .route("/files", web::get().to(list_files))
            .route("/search", web::get().to(search))
            .route("/daily", web::get().to(get_daily_log))
            .route("/daily", web::post().to(append_daily_log))
            .route("/long-term", web::get().to(get_long_term))
            .route("/long-term", web::post().to(append_long_term))
            .route("/stats", web::get().to(get_stats))
            .route("/reindex", web::post().to(reindex))
            .route("/info", web::get().to(memory_info))
            // Phase 1: Memory System Overhaul endpoints
            .route("/graph", web::get().to(get_graph))
            .route("/associations", web::post().to(create_association))
            .route("/associations", web::get().to(list_associations))
            .route("/associations/{id}", web::delete().to(delete_association))
            .route("/hybrid-search", web::get().to(hybrid_search))
            .route("/embeddings/stats", web::get().to(embedding_stats))
            .route("/embeddings/backfill", web::post().to(backfill_embeddings))
            .route("/associations/rebuild", web::post().to(rebuild_associations))
            .route("/all", web::delete().to(delete_all_memories))
            // Phase 2: Dedup, merge, export/import
            .route("/merge", web::post().to(merge_memories))
            .route("/export", web::get().to(export_memories))
            .route("/import", web::post().to(import_memories)),
    );
}
