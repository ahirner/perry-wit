//! TypeScript configuration generator for zero-config IDE integration.

/// Generates a standard `tsconfig.json` content configured for Perry TypeScript components.
pub fn generate_default_tsconfig() -> String {
    r#"{
  "compilerOptions": {
    "target": "ES2022",
    "module": "ESNext",
    "moduleResolution": "bundler",
    "strict": true,
    "noEmit": true,
    "skipLibCheck": true,
    "typeRoots": [
      "./.perry/types",
      "./node_modules/@types"
    ]
  },
  "include": [
    "src/**/*",
    "examples/**/*",
    "*.ts",
    ".perry/types/**/*"
  ]
}
"#
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_default_tsconfig_is_valid_json() {
        let content = generate_default_tsconfig();
        let val: serde_json::Value =
            serde_json::from_str(&content).expect("tsconfig must be valid json");
        assert!(val.get("compilerOptions").is_some());
        assert!(val.get("include").is_some());
    }
}
