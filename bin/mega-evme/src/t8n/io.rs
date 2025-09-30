use std::{fs, io::Read, path::PathBuf};

use state_test::types::Env;

use crate::t8n::{
    Result, StateAlloc, StdinInput, T8nError, Transaction, TransitionInputs, TransitionResults,
};

/// Load prestate allocation from a JSON file
pub fn load_alloc(path: &str) -> Result<StateAlloc> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| T8nError::InputLoad { file: path.to_string(), source: e })?;

    let alloc: StateAlloc = serde_json::from_str(&content)
        .map_err(|e| T8nError::JsonParse { file: path.to_string(), source: e })?;

    Ok(alloc)
}

/// Load environment from a JSON file
pub fn load_env(path: &str) -> Result<Env> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| T8nError::InputLoad { file: path.to_string(), source: e })?;

    let env: Env = serde_json::from_str(&content)
        .map_err(|e| T8nError::JsonParse { file: path.to_string(), source: e })?;

    Ok(env)
}

/// Load transactions from a JSON file
pub fn load_transactions(path: &str) -> Result<Vec<Transaction>> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| T8nError::InputLoad { file: path.to_string(), source: e })?;

    let txs: Vec<Transaction> = serde_json::from_str(&content)
        .map_err(|e| T8nError::JsonParse { file: path.to_string(), source: e })?;

    Ok(txs)
}

/// Load inputs from stdin in combined JSON format
pub fn load_from_stdin() -> Result<TransitionInputs> {
    let mut buffer = String::new();
    std::io::stdin()
        .read_to_string(&mut buffer)
        .map_err(|e| T8nError::InputLoad { file: "stdin".to_string(), source: e })?;

    let stdin_input: StdinInput = serde_json::from_str(&buffer)
        .map_err(|e| T8nError::JsonParse { file: "stdin".to_string(), source: e })?;

    Ok(TransitionInputs { alloc: stdin_input.alloc, env: stdin_input.env, txs: stdin_input.txs })
}

/// Write the post-state alloc to a file
pub fn write_alloc_to_file(
    post_state_alloc: &StateAlloc,
    output_alloc: &str,
    output_basedir: Option<&PathBuf>,
) -> Result<()> {
    let output_path = if let Some(base_dir) = output_basedir {
        base_dir.join(output_alloc)
    } else {
        PathBuf::from(output_alloc)
    };

    // Convert to serializable format
    let json_output = serde_json::to_string_pretty(post_state_alloc)
        .map_err(|e| T8nError::JsonParse { file: output_path.display().to_string(), source: e })?;

    // Create base directory if it doesn't exist
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| T8nError::OutputWrite { file: parent.display().to_string(), source: e })?;
    }

    fs::write(&output_path, &json_output).map_err(|e| T8nError::OutputWrite {
        file: output_path.display().to_string(),
        source: e,
    })?;

    Ok(())
}

/// Write the execution result to a file
pub fn write_result_to_file(
    results: &TransitionResults,
    output_result: &str,
    output_basedir: Option<&PathBuf>,
) -> Result<()> {
    let output_path = if let Some(base_dir) = output_basedir {
        base_dir.join(output_result)
    } else {
        PathBuf::from(output_result)
    };

    let json_output = serde_json::to_string_pretty(results)
        .map_err(|e| T8nError::JsonParse { file: output_path.display().to_string(), source: e })?;

    // Create base directory if it doesn't exist
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| T8nError::OutputWrite { file: parent.display().to_string(), source: e })?;
    }

    fs::write(&output_path, &json_output).map_err(|e| T8nError::OutputWrite {
        file: output_path.display().to_string(),
        source: e,
    })?;

    Ok(())
}

/// Write the transaction body RLP to output file
pub fn write_body_output(_body_file: &str, _output_basedir: Option<&PathBuf>) -> Result<()> {
    // TODO: Generate RLP-encoded transaction body
    // eprintln!("body RLP output not yet implemented");
    Ok(())
}
