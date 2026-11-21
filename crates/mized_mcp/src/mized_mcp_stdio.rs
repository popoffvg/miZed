#[cfg(unix)]
fn main() -> anyhow::Result<()> {
    unix::run_mcp_cli()
}

#[cfg(unix)]
mod unix {
    use std::{
        ffi::OsStr,
        fs,
        io::{self, BufRead as _, BufReader, Write as _},
        os::unix::{ffi::OsStrExt as _, net::UnixStream},
        path::Path,
        thread,
    };

    use anyhow::{Context as _, Result, bail, ensure};
    use serde_json::{Value, json};

    const USAGE: &str = "Usage: mized-mcp ADDRESS_FILE [list | call TOOL [JSON | -]]\n\
        Pass - to read the tool arguments from stdin.\n\
        Pass only ADDRESS_FILE to start the MCP stdio bridge.";

    pub(super) fn run_mcp_cli() -> Result<()> {
        let mut arguments = std::env::args_os().skip(1);
        let address_file = arguments.next().context(USAGE)?;
        let command = arguments.next();
        if address_file == "--help" || command.as_ref().is_some_and(|command| command == "--help") {
            writeln!(io::stdout().lock(), "{USAGE}")?;
            return Ok(());
        }
        let request = match command {
            None => None,
            Some(command) if command == "list" && arguments.next().is_none() => {
                Some(("tools/list", json!({})))
            }
            Some(command) if command == "call" => {
                let name = arguments.next().context(USAGE)?;
                let name = name.to_str().context("Use a UTF-8 miZed MCP tool name")?;
                let input = arguments.next();
                ensure!(arguments.next().is_none(), "{USAGE}");
                let input: Value = match input.as_deref() {
                    None => json!({}),
                    Some(input) if input == "-" => serde_json::from_reader(io::stdin().lock())
                        .context("Could not read miZed MCP tool arguments from stdin")?,
                    Some(input) => serde_json::from_slice(input.as_bytes())
                        .context("Could not parse miZed MCP tool arguments")?,
                };
                ensure!(
                    input.is_object(),
                    "Pass miZed MCP tool arguments as a JSON object"
                );
                Some(("tools/call", json!({ "name": name, "arguments": input })))
            }
            _ => bail!("{USAGE}"),
        };

        let socket = connect_mcp_socket(Path::new(&address_file))?;
        match request {
            Some((method, parameters)) => run_mcp_request(socket, method, parameters),
            None => bridge_mcp_stdio(socket),
        }
    }

    fn connect_mcp_socket(address_file: &Path) -> Result<UnixStream> {
        let address = fs::read(address_file).with_context(|| {
            format!(
                "Could not read the miZed MCP address from {}",
                address_file.display()
            )
        })?;
        let socket_path = Path::new(OsStr::from_bytes(&address));
        UnixStream::connect(socket_path).with_context(|| {
            format!(
                "Could not connect to miZed MCP at {}",
                socket_path.display()
            )
        })
    }

    fn bridge_mcp_stdio(mut output: UnixStream) -> Result<()> {
        let mut input = output.try_clone()?;
        thread::spawn(move || {
            if let Err(error) = io::copy(&mut io::stdin().lock(), &mut input)
                .and_then(|_| input.shutdown(std::net::Shutdown::Write))
            {
                eprintln!("Could not send miZed MCP input: {error}");
            }
        });
        io::copy(&mut output, &mut io::stdout().lock())
            .context("Could not read miZed MCP output")?;
        Ok(())
    }

    fn run_mcp_request(socket: UnixStream, method: &str, parameters: Value) -> Result<()> {
        let mut socket = BufReader::new(socket);
        read_mcp_result(
            &mut socket,
            1,
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "mized-mcp", "version": env!("CARGO_PKG_VERSION") }
            }),
        )?;
        writeln!(
            socket.get_mut(),
            "{}",
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
        )?;
        let result = read_mcp_result(&mut socket, 2, method, parameters)?;
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            bail!("miZed MCP tool failed: {}", result["content"]);
        }
        let mut output = io::stdout().lock();
        serde_json::to_writer(
            &mut output,
            result.get("structuredContent").unwrap_or(&result),
        )?;
        writeln!(output)?;
        Ok(())
    }

    fn read_mcp_result(
        socket: &mut BufReader<UnixStream>,
        id: u8,
        method: &str,
        parameters: Value,
    ) -> Result<Value> {
        writeln!(
            socket.get_mut(),
            "{}",
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": parameters })
        )?;
        let mut line = String::new();
        loop {
            line.clear();
            ensure!(
                socket.read_line(&mut line)? != 0,
                "miZed MCP disconnected during {method}"
            );
            let mut response: Value =
                serde_json::from_str(&line).context("Could not parse the miZed MCP response")?;
            if response.get("id").and_then(Value::as_u64) != Some(u64::from(id)) {
                continue;
            }
            if let Some(error) = response.get("error") {
                bail!("miZed MCP request {method} failed: {error}");
            }
            return response
                .as_object_mut()
                .and_then(|response| response.remove("result"))
                .context("The miZed MCP response has no result");
        }
    }
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("miZed MCP requires a Unix socket");
    std::process::ExitCode::FAILURE
}
