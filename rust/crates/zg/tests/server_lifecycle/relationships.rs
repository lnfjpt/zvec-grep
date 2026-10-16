use super::*;

#[test]
fn persisted_calls_match_across_direct_daemon_and_mcp() -> Result<(), Box<dyn Error>> {
    let _permit = server_test_permit();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let embedding = EmbeddingServer::start()?;
    std::fs::write(
        workspace.path().join("calls.py"),
        "def target():\n    pass\n\ndef caller():\n    target()\n",
    )?;
    let indexed = Command::new(&binary)
        .current_dir(workspace.path())
        .env("ZVEC_GREP_HOME", home.path())
        .env(
            "ZVEC_GREP_WORKSPACE_REGISTRY",
            home.path().join("workspaces.json"),
        )
        .env("ZVEC_GREP_API_KEY", "local-test-key")
        .args([
            "--index",
            "--mode",
            "direct",
            "--allow-remote",
            "--embedding",
            "qwen/text-embedding-v4",
            "--endpoint",
        ])
        .arg(format!("http://{}/embeddings", embedding.address))
        .output()?;
    assert_command_success(&indexed);
    // Relationship reads must work with the embedding service shut down.
    drop(embedding);
    let (mut guard, _) = start_server(&binary, &home, "agent", None, |_| {})?;
    let mut bridge =
        StdioBridge::spawn_with_toolset(&binary, home.path(), &guard.listen, Some("agent"))?;
    bridge.request(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-03-26", "capabilities": {},
            "clientInfo": {"name": "relationships-test", "version": "1"}}
    }))?;
    bridge.notify(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
    for (id, tool, symbol, endpoint) in [
        (2, "callers", "target", "caller"),
        (3, "callees", "caller", "target"),
    ] {
        let mut replies = Vec::new();
        for mode in ["direct", "server"] {
            let output = Command::new(&binary)
                .arg(format!("--{tool}"))
                .arg(symbol)
                .arg(workspace.path())
                .args(["--mode", mode, "--home"])
                .arg(home.path())
                .output()?;
            assert_command_success(&output);
            let reply: serde_json::Value = serde_json::from_slice(&output.stdout)?;
            assert_eq!(reply.as_array().ok_or("matches array")?.len(), 1);
            assert_eq!(reply[0]["name"], symbol);
            assert_eq!(reply[0]["totalEdges"], 1);
            assert_eq!(reply[0]["edges"][0]["symbol"]["name"], endpoint);
            assert_eq!(reply[0]["edges"][0]["line"], 5);
            replies.push(reply);
        }
        assert_eq!(replies[0], replies[1]);
        let mcp = bridge.request(&json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": tool, "arguments": {"root": workspace.path(), "symbol": symbol}}
        }))?;
        assert_eq!(mcp["result"]["structuredContent"]["matches"], replies[0]);
    }
    bridge.close()?;
    assert_command_success(&guard.stop()?);
    Ok(())
}
