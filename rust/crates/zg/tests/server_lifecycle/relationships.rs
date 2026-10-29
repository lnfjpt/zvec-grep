use super::*;

#[test]
fn invalid_relationship_queries_do_not_start_a_daemon() -> Result<(), Box<dyn Error>> {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let directory = TempDir::new()?;
    let home = directory.path().join("daemon-home");
    for (symbol, limit, message) in [
        (" ", "1", "symbol must contain"),
        ("target", "0", "relationship limit must be positive"),
    ] {
        for tool in ["--callers", "--callees"] {
            let output = Command::new(&binary)
                .args([tool, symbol, "--limit", limit, "--mode", "auto", "--home"])
                .arg(&home)
                .current_dir(directory.path())
                .env("ZVEC_GREP_HOME", &home)
                .output()?;
            assert!(!output.status.success());
            assert!(String::from_utf8_lossy(&output.stderr).contains(message));
            assert!(
                !home.exists(),
                "invalid input must not initialize daemon state"
            );
        }
    }
    Ok(())
}

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
    std::fs::create_dir(workspace.path().join("src"))?;
    std::fs::write(
        workspace.path().join("src/main.py"),
        "def nested_entry():\n    pass\n",
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
    // Indexing resolves the intra-file call, so the graph already holds it.
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
                .arg(format!(" {symbol} "))
                .arg(workspace.path())
                .args(["--mode", mode, "--home"])
                .arg(home.path())
                .output()?;
            assert_command_success(&output);
            let reply: serde_json::Value = serde_json::from_slice(&output.stdout)?;
            assert_eq!(reply["matches"].as_array().ok_or("matches array")?.len(), 1);
            assert_eq!(reply["suggestions"], json!([]));
            assert_eq!(reply["matches"][0]["name"], symbol);
            assert_eq!(reply["matches"][0]["truncated"], false);
            assert_eq!(reply["matches"][0]["edges"][0]["symbol"]["name"], endpoint);
            assert_eq!(reply["matches"][0]["edges"][0]["line"], 5);
            assert_eq!(reply["matches"][0]["edges"][0]["filePath"], "calls.py");
            replies.push(reply);
        }
        assert_eq!(replies[0], replies[1]);
        let mcp = bridge.request(&json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": format!("zvec_grep_{tool}"), "arguments": {"root": workspace.path(), "symbol": format!(" {symbol} ")}}
        }))?;
        assert_eq!(mcp["result"]["structuredContent"], replies[0]);
    }
    assert_suggestions(&binary, home.path(), workspace.path(), &mut bridge)?;
    assert_path_filters(&binary, home.path(), workspace.path(), &mut bridge)?;
    bridge.close()?;
    assert_command_success(&guard.stop()?);
    Ok(())
}

// Cross-file edges and edge truncation are only produced by the resolver sweep and
// limit admission, so they must be verified through the same three read paths.
#[test]
fn cross_file_and_truncated_calls_match_across_direct_daemon_and_mcp() -> Result<(), Box<dyn Error>>
{
    let _permit = server_test_permit();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let embedding = EmbeddingServer::start()?;
    std::fs::create_dir(workspace.path().join("foo"))?;
    std::fs::write(
        workspace.path().join("lib.rs"),
        concat!(
            "mod foo;\n",
            "use foo::callee;\n",
            "\n",
            "fn caller_one() {\n    callee();\n}\n",
            "\n",
            "fn caller_two() {\n    callee();\n}\n",
            "\n",
            "fn caller_three() {\n    callee();\n}\n",
        ),
    )?;
    std::fs::write(workspace.path().join("foo/mod.rs"), "pub fn callee() {}\n")?;
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
    // Cross-file calls resolve during indexing; reads must not need the service.
    drop(embedding);
    let (mut guard, _) = start_server(&binary, &home, "agent", None, |_| {})?;
    let mut bridge = start_agent_bridge(&binary, home.path(), &guard.listen)?;
    assert_cross_file_edges(&binary, home.path(), workspace.path(), &mut bridge)?;
    assert_truncated_edges(&binary, home.path(), workspace.path(), &mut bridge)?;
    bridge.close()?;
    assert_command_success(&guard.stop()?);
    Ok(())
}

#[test]
fn unindexed_workspace_reports_missing_index_across_direct_and_mcp() -> Result<(), Box<dyn Error>> {
    let _permit = server_test_permit();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let workspace = TempDir::new()?;
    std::fs::write(
        workspace.path().join("calls.py"),
        "def target():\n    pass\n",
    )?;
    // An unindexed workspace is an error, never an empty result set.
    for tool in ["--callers", "--callees"] {
        let output = Command::new(&binary)
            .current_dir(workspace.path())
            .args([tool, "target"])
            .arg(workspace.path())
            .args(["--mode", "direct", "--home"])
            .arg(home.path())
            .output()?;
        assert!(
            !output.status.success(),
            "{tool} on an unindexed workspace must fail"
        );
        assert!(
            output.stdout.is_empty(),
            "{tool} stdout: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("workspace index is unavailable"),
            "{tool} stderr: {stderr}"
        );
    }
    let (mut guard, _) = start_server(&binary, &home, "agent", None, |_| {})?;
    let mut bridge = start_agent_bridge(&binary, home.path(), &guard.listen)?;
    for (id, tool) in [(2, "callers"), (3, "callees")] {
        let mcp = bridge.request(&json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": format!("zvec_grep_{tool}"), "arguments": {"root": workspace.path(), "symbol": "target"}}
        }))?;
        assert_eq!(mcp["result"]["isError"], true, "{tool}: {mcp}");
        let text = mcp["result"]["content"][0]["text"]
            .as_str()
            .ok_or("error text")?;
        assert!(
            text.contains("workspace index is unavailable"),
            "{tool} text: {text}"
        );
        assert!(text.contains("retryable: false"), "{tool} text: {text}");
    }
    bridge.close()?;
    assert_command_success(&guard.stop()?);
    Ok(())
}

fn start_agent_bridge(
    binary: &Path,
    home: &Path,
    listen: &str,
) -> Result<StdioBridge, Box<dyn Error>> {
    let mut bridge = StdioBridge::spawn_with_toolset(binary, home, listen, Some("agent"))?;
    bridge.request(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-03-26", "capabilities": {},
            "clientInfo": {"name": "relationships-test", "version": "1"}}
    }))?;
    bridge.notify(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
    Ok(bridge)
}

// Runs the same query once through the local engine and once through the daemon.
fn relationship_replies(
    binary: &Path,
    home: &Path,
    workspace: &Path,
    args: &[&str],
) -> Result<Vec<serde_json::Value>, Box<dyn Error>> {
    let mut replies = Vec::new();
    for mode in ["direct", "server"] {
        let output = Command::new(binary)
            .current_dir(workspace)
            .args(args)
            .arg(workspace)
            .args(["--mode", mode, "--home"])
            .arg(home)
            .output()?;
        assert_command_success(&output);
        replies.push(serde_json::from_slice(&output.stdout)?);
    }
    Ok(replies)
}

fn assert_cross_file_edges(
    binary: &Path,
    home: &Path,
    workspace: &Path,
    bridge: &mut StdioBridge,
) -> Result<(), Box<dyn Error>> {
    let incoming = relationship_replies(binary, home, workspace, &["--callers", "callee"])?;
    assert_eq!(incoming[0], incoming[1], "direct and server callers differ");
    let matches = incoming[0]["matches"].as_array().ok_or("callers matches")?;
    assert_eq!(matches.len(), 1, "callers: {:?}", incoming[0]);
    assert_eq!(matches[0]["name"], "callee");
    assert_eq!(
        matches[0]["filePath"], "foo/mod.rs",
        "cross-file definition path"
    );
    let edges = matches[0]["edges"].as_array().ok_or("caller edges")?;
    assert_eq!(edges.len(), 3, "callers: {:?}", incoming[0]);
    for edge in edges {
        assert_eq!(edge["filePath"], "lib.rs", "call site path");
    }
    let mut names: Vec<&str> = edges
        .iter()
        .map(|edge| edge["symbol"]["name"].as_str().unwrap_or_default())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["caller_one", "caller_three", "caller_two"]);
    let mcp = bridge.request(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "zvec_grep_callers", "arguments": {"root": workspace, "symbol": "callee"}}
    }))?;
    assert_eq!(mcp["result"]["structuredContent"], incoming[0]);

    let outgoing = relationship_replies(binary, home, workspace, &["--callees", "caller_one"])?;
    assert_eq!(outgoing[0], outgoing[1], "direct and server callees differ");
    let matches = outgoing[0]["matches"].as_array().ok_or("callees matches")?;
    assert_eq!(matches.len(), 1, "callees: {:?}", outgoing[0]);
    assert_eq!(matches[0]["filePath"], "lib.rs");
    let edges = matches[0]["edges"].as_array().ok_or("callee edges")?;
    assert_eq!(edges.len(), 1, "callees: {:?}", outgoing[0]);
    assert_eq!(edges[0]["symbol"]["name"], "callee");
    assert_eq!(edges[0]["symbol"]["filePath"], "foo/mod.rs");
    assert_eq!(edges[0]["line"], 5, "call line inside caller_one");
    let mcp = bridge.request(&json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {"name": "zvec_grep_callees", "arguments": {"root": workspace, "symbol": "caller_one"}}
    }))?;
    assert_eq!(mcp["result"]["structuredContent"], outgoing[0]);
    Ok(())
}

fn assert_truncated_edges(
    binary: &Path,
    home: &Path,
    workspace: &Path,
    bridge: &mut StdioBridge,
) -> Result<(), Box<dyn Error>> {
    let replies = relationship_replies(
        binary,
        home,
        workspace,
        &["--callers", "callee", "--limit", "2"],
    )?;
    assert_eq!(replies[0], replies[1], "direct and server limits differ");
    let matches = replies[0]["matches"].as_array().ok_or("limited matches")?;
    assert_eq!(matches.len(), 1, "limited: {:?}", replies[0]);
    let edges = matches[0]["edges"].as_array().ok_or("limited edges")?;
    assert_eq!(edges.len(), 2, "limit must cap edges: {:?}", replies[0]);
    assert_eq!(
        matches[0]["truncated"], true,
        "limit must mark truncation: {:?}",
        replies[0]
    );
    let mcp = bridge.request(&json!({
        "jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": {"name": "zvec_grep_callers", "arguments": {"root": workspace, "symbol": "callee", "limit": 2}}
    }))?;
    assert_eq!(mcp["result"]["structuredContent"], replies[0]);
    Ok(())
}

fn assert_path_filters(
    binary: &Path,
    home: &Path,
    workspace: &Path,
    bridge: &mut StdioBridge,
) -> Result<(), Box<dyn Error>> {
    let nested = workspace.join("src");
    std::fs::create_dir_all(&nested)?;
    for (id, path, expected) in [(50, "calls.py", 1), (51, "missing.py", 0)] {
        for mode in ["direct", "server"] {
            let output = Command::new(binary)
                .current_dir(workspace)
                .args(["--callers", "target"])
                .arg(workspace)
                .args(["--path", path, "--mode", mode, "--home"])
                .arg(home)
                .output()?;
            assert_command_success(&output);
            let reply: serde_json::Value = serde_json::from_slice(&output.stdout)?;
            assert_eq!(
                reply["matches"].as_array().ok_or("matches")?.len(),
                expected
            );
        }
        let mcp = bridge.request(&json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": "zvec_grep_callers", "arguments": {"root": workspace, "symbol": "target", "path": path}}
        }))?;
        assert_eq!(
            mcp["result"]["structuredContent"]["matches"]
                .as_array()
                .ok_or("MCP matches")?
                .len(),
            expected
        );
    }
    for (mode, symbol, path, expected_path, suggestions) in [
        ("direct", "nested_entry", "main.py", "src/main.py", false),
        ("server", "nested_entry", "main.py", "src/main.py", false),
        ("server", "NESTED_ENTRY", "main.py", "src/main.py", true),
        ("direct", "target", "../calls.py", "calls.py", false),
    ] {
        let output = Command::new(binary)
            .current_dir(&nested)
            .args([
                "--callers",
                symbol,
                "--path",
                path,
                "--mode",
                mode,
                "--home",
            ])
            .arg(home)
            .output()?;
        assert_command_success(&output);
        let reply: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        let values = reply[if suggestions {
            "suggestions"
        } else {
            "matches"
        }]
        .as_array()
        .ok_or("filtered results")?;
        assert_eq!(values.len(), 1);
        assert_eq!(
            Path::new(values[0]["filePath"].as_str().ok_or("file path")?),
            Path::new(expected_path)
        );
    }
    Ok(())
}

fn assert_suggestions(
    binary: &Path,
    home: &Path,
    workspace: &Path,
    bridge: &mut StdioBridge,
) -> Result<(), Box<dyn Error>> {
    let output = Command::new(binary)
        .args(["--callers", "WrongScope::TARGET"])
        .arg(workspace)
        .args(["--mode", "direct", "--home"])
        .arg(home)
        .output()?;
    assert_command_success(&output);
    let reply: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(reply["matches"], json!([]));
    assert_eq!(
        reply["suggestions"].as_array().ok_or("suggestions")?.len(),
        1
    );
    assert_eq!(reply["suggestions"][0]["name"], "target");
    let mcp = bridge.request(&json!({
        "jsonrpc": "2.0", "id": 20, "method": "tools/call",
        "params": {"name": "zvec_grep_callers", "arguments": {"root": workspace, "symbol": "WrongScope::TARGET"}}
    }))?;
    assert_eq!(mcp["result"]["structuredContent"], reply);
    let text = mcp["result"]["content"][0]["text"].as_str().ok_or("text")?;
    assert_eq!(serde_json::from_str::<serde_json::Value>(text)?, reply);
    Ok(())
}
