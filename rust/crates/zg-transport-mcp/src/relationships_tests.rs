use std::sync::{Arc, Mutex};

use rmcp::{ServiceExt, model::CallToolRequestParams};
use serde_json::json;
use zg_engine::api::relationships::{RelationshipEdge, RelationshipSymbol};

use super::*;

fn root() -> String {
    std::env::temp_dir().to_string_lossy().into_owned()
}

#[test]
fn relationship_inputs_trim_symbols_and_reject_unsafe_or_unknown_options() {
    let request = RelationshipInput {
        root: root(),
        symbol: "  Class::method  ".into(),
        limit: None,
    }
    .into_request()
    .expect("valid relationship request");
    assert_eq!(request.symbol, "Class::method");
    assert_eq!(request.limit, Some(20));
    for input in [
        json!({"root": root(), "symbol": " "}),
        json!({"root": root(), "symbol": "x".repeat(1025)}),
        json!({"root": "relative", "symbol": "run"}),
        json!({"root": format!("{}/../other", root()), "symbol": "run"}),
        json!({"root": root(), "symbol": "run", "limit": 0}),
    ] {
        let input: RelationshipInput = serde_json::from_value(input).expect("input shape");
        assert!(input.into_request().is_err());
    }
    for input in [
        json!({"root": root(), "symbol": "run", "limit": -1}),
        json!({"root": root(), "symbol": "run", "autoUpdate": true}),
        json!({"root": root(), "symbol": "run", "refresh": "wait"}),
    ] {
        assert!(serde_json::from_value::<RelationshipInput>(input).is_err());
    }
}

#[test]
fn relationship_tools_advertise_read_only_schemas_in_both_toolsets() {
    struct Status;
    impl ServerStatusProvider for Status {
        fn snapshot(&self) -> ServerStatusSnapshot {
            ServerStatusSnapshot::default()
        }
    }
    for server in [
        ZvecGrepMcpServer::agent_direct(Arc::new(ZvecGrep::new())),
        ZvecGrepMcpServer::full_direct(Arc::new(ZvecGrep::new()), Arc::new(Status)),
    ] {
        for name in ["callers", "callees"] {
            let tool = server
                .listed_tools()
                .into_iter()
                .find(|tool| tool.name == name)
                .expect("relationship tool");
            let value = serde_json::to_value(tool).expect("tool schema");
            assert_eq!(value["annotations"]["readOnlyHint"], true);
            assert_eq!(value["annotations"]["openWorldHint"], false);
            assert_eq!(value["inputSchema"]["additionalProperties"], false);
            assert!(value["outputSchema"]["properties"]["matches"].is_object());
        }
    }
}

struct RelationshipsOnly {
    calls: Mutex<Vec<(bool, RelationshipOptions)>>,
}

#[async_trait]
impl IndexOperationProvider for RelationshipsOnly {
    async fn submit_index(
        &self,
        _: IndexOptions,
        _: bool,
    ) -> Result<IndexOperationResult, EngineError> {
        panic!("relationship reads must not submit index jobs")
    }

    async fn drop_index(&self, _: InfoOptions) -> Result<bool, EngineError> {
        panic!("relationship reads must not drop indexes")
    }

    async fn callers(
        &self,
        _: &ZvecGrep,
        request: RelationshipOptions,
    ) -> Result<Vec<SymbolRelationships>, EngineError> {
        Ok(self.read(true, request))
    }

    async fn callees(
        &self,
        _: &ZvecGrep,
        request: RelationshipOptions,
    ) -> Result<Vec<SymbolRelationships>, EngineError> {
        Ok(self.read(false, request))
    }
}

impl RelationshipsOnly {
    fn read(&self, incoming: bool, request: RelationshipOptions) -> Vec<SymbolRelationships> {
        assert!(
            request
                .signal
                .as_ref()
                .is_some_and(|signal| !signal.is_cancelled())
        );
        let matches = vec![SymbolRelationships {
            symbol: RelationshipSymbol {
                name: request.symbol.clone(),
                file_path: "src/subject.rs".into(),
                start_line: 3,
                end_line: 8,
            },
            total_edges: 7,
            edges: vec![
                RelationshipEdge {
                    symbol: Some(RelationshipSymbol {
                        name: if incoming { "Caller" } else { "Callee" }.into(),
                        file_path: "src/edge.rs".into(),
                        start_line: 10,
                        end_line: 20,
                    }),
                    line: Some(12),
                    column: Some(0),
                },
                RelationshipEdge {
                    symbol: None,
                    line: None,
                    column: None,
                },
            ],
        }];
        self.calls
            .lock()
            .expect("calls lock")
            .push((incoming, request));
        matches
    }
}

#[tokio::test]
async fn relationship_wire_calls_route_without_indexing_and_return_matching_json() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let provider = Arc::new(RelationshipsOnly {
            calls: Mutex::new(Vec::new()),
        });
        let server = ZvecGrepMcpServer::build_with_index_operations(
            Arc::new(ZvecGrep::new()),
            McpToolset::Agent,
            None,
            provider.clone(),
        );
        let (server_io, client_io) = tokio::io::duplex(8192);
        let task = tokio::spawn(async move {
            server
                .serve(server_io)
                .await
                .expect("server")
                .waiting()
                .await
        });
        let client = ().serve(client_io).await.expect("client");
        for (name, endpoint) in [("callers", "Caller"), ("callees", "Callee")] {
            let response = client
                .call_tool(
                    CallToolRequestParams::new(name).with_arguments(
                        json!({"root": root(), "symbol": " Subject::run ", "limit": 2})
                            .as_object()
                            .expect("arguments")
                            .clone(),
                    ),
                )
                .await
                .expect("relationship call");
            let value = serde_json::to_value(response).expect("response JSON");
            assert_ne!(value["isError"], true);
            let matches = &value["structuredContent"]["matches"];
            assert_eq!(matches[0]["name"], "Subject::run");
            assert_eq!(matches[0]["filePath"], "src/subject.rs");
            assert_eq!(matches[0]["totalEdges"], 7);
            assert_eq!(matches[0]["edges"][0]["symbol"]["name"], endpoint);
            assert_eq!(matches[0]["edges"][0]["column"], 0);
            assert!(matches[0]["edges"][1]["symbol"].is_null());
            let text = value["content"][0]["text"].as_str().expect("text result");
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(text).expect("text JSON"),
                *matches
            );
        }
        {
            let calls = provider.calls.lock().expect("recorded calls");
            assert_eq!(calls.len(), 2);
            assert!(calls[0].0);
            assert!(!calls[1].0);
            assert!(calls.iter().all(|(_, request)| request.limit == Some(2)));
        }
        client.cancel().await.expect("close client");
        task.await.expect("server task").expect("server stop");
    })
    .await
    .expect("MCP test terminates");
}
