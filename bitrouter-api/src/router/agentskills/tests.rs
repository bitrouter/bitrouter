//! Tests for `/v1/skills` CRUD endpoints.

use std::sync::Arc;

use bitrouter_core::routers::registry::{SkillEntry, SkillService};
use tokio::sync::RwLock;
use warp::Filter;

use super::filters::skills_filter;

// ── Mock service ───────────────────────────────────────────────────

struct MockSkillService {
    skills: RwLock<Vec<SkillEntry>>,
}

impl MockSkillService {
    fn new() -> Self {
        Self {
            skills: RwLock::new(Vec::new()),
        }
    }
}

impl SkillService for MockSkillService {
    async fn create(
        &self,
        name: String,
        description: String,
        source: Option<String>,
        required_apis: Vec<String>,
    ) -> Result<SkillEntry, String> {
        let mut skills = self.skills.write().await;
        if skills.iter().any(|s| s.name == name) {
            return Err(format!("skill '{name}' already exists"));
        }
        let entry = SkillEntry {
            id: format!("skill-{}", skills.len()),
            name,
            description,
            source: source.unwrap_or_else(|| "manual".to_string()),
            required_apis,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        };
        skills.push(entry.clone());
        Ok(entry)
    }

    async fn list(&self) -> Result<Vec<SkillEntry>, String> {
        Ok(self.skills.read().await.clone())
    }

    async fn get(&self, name: &str) -> Result<Option<SkillEntry>, String> {
        Ok(self
            .skills
            .read()
            .await
            .iter()
            .find(|s| s.name == name)
            .cloned())
    }

    async fn delete(&self, name: &str) -> Result<bool, String> {
        let mut skills = self.skills.write().await;
        let len_before = skills.len();
        skills.retain(|s| s.name != name);
        Ok(skills.len() < len_before)
    }
}

// ── Helpers ────────────────────────────────────────────────────────

fn make_filter() -> (
    Arc<MockSkillService>,
    impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone,
) {
    let svc = Arc::new(MockSkillService::new());
    let filter = skills_filter(svc.clone());
    (svc, filter)
}

// ── Create tests ───────────────────────────────────────────────────

#[tokio::test]
async fn create_skill_returns_201() {
    let (_, filter) = make_filter();
    let resp = warp::test::request()
        .method("POST")
        .path("/v1/skills")
        .json(&serde_json::json!({
            "name": "web-search",
            "description": "Search the web"
        }))
        .reply(&filter)
        .await;
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = serde_json::from_slice(resp.body()).expect("parse body");
    assert_eq!(body["name"], "web-search");
    assert_eq!(body["type"], "skill");
    assert_eq!(body["source"], "manual");
}

#[tokio::test]
async fn create_duplicate_skill_returns_400() {
    let (svc, filter) = make_filter();
    svc.create("web-search".to_string(), "Search".to_string(), None, vec![])
        .await
        .expect("seed");

    let resp = warp::test::request()
        .method("POST")
        .path("/v1/skills")
        .json(&serde_json::json!({
            "name": "web-search",
            "description": "Search the web"
        }))
        .reply(&filter)
        .await;
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = serde_json::from_slice(resp.body()).expect("parse body");
    assert_eq!(body["error"]["type"], "invalid_request_error");
}

// ── List tests ─────────────────────────────────────────────────────

#[tokio::test]
async fn list_skills_empty() {
    let (_, filter) = make_filter();
    let resp = warp::test::request()
        .method("GET")
        .path("/v1/skills")
        .reply(&filter)
        .await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = serde_json::from_slice(resp.body()).expect("parse body");
    assert_eq!(body["data"].as_array().expect("data array").len(), 0);
}

#[tokio::test]
async fn list_skills_returns_created() {
    let (svc, filter) = make_filter();
    svc.create("a".to_string(), "A".to_string(), None, vec![])
        .await
        .expect("seed");
    svc.create("b".to_string(), "B".to_string(), None, vec![])
        .await
        .expect("seed");

    let resp = warp::test::request()
        .method("GET")
        .path("/v1/skills")
        .reply(&filter)
        .await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = serde_json::from_slice(resp.body()).expect("parse body");
    assert_eq!(body["data"].as_array().expect("data array").len(), 2);
}

// ── Get tests ──────────────────────────────────────────────────────

#[tokio::test]
async fn get_skill_found() {
    let (svc, filter) = make_filter();
    svc.create("web-search".to_string(), "Search".to_string(), None, vec![])
        .await
        .expect("seed");

    let resp = warp::test::request()
        .method("GET")
        .path("/v1/skills/web-search")
        .reply(&filter)
        .await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = serde_json::from_slice(resp.body()).expect("parse body");
    assert_eq!(body["name"], "web-search");
}

#[tokio::test]
async fn get_skill_not_found() {
    let (_, filter) = make_filter();
    let resp = warp::test::request()
        .method("GET")
        .path("/v1/skills/nonexistent")
        .reply(&filter)
        .await;
    assert_eq!(resp.status(), 404);
    let body: serde_json::Value = serde_json::from_slice(resp.body()).expect("parse body");
    assert_eq!(body["error"]["type"], "not_found_error");
}

// ── Delete tests ───────────────────────────────────────────────────

#[tokio::test]
async fn delete_skill_found() {
    let (svc, filter) = make_filter();
    svc.create("web-search".to_string(), "Search".to_string(), None, vec![])
        .await
        .expect("seed");

    let resp = warp::test::request()
        .method("DELETE")
        .path("/v1/skills/web-search")
        .reply(&filter)
        .await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = serde_json::from_slice(resp.body()).expect("parse body");
    assert_eq!(body["type"], "skill_deleted");
}

#[tokio::test]
async fn delete_skill_not_found() {
    let (_, filter) = make_filter();
    let resp = warp::test::request()
        .method("DELETE")
        .path("/v1/skills/nonexistent")
        .reply(&filter)
        .await;
    assert_eq!(resp.status(), 404);
    let body: serde_json::Value = serde_json::from_slice(resp.body()).expect("parse body");
    assert_eq!(body["error"]["type"], "not_found_error");
}
