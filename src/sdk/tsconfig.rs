//! TypeScript configuration generator for zero-config IDE integration.

/// Generates a standard `tsconfig.json` content configured for Perry TypeScript components.
pub fn generate_default_tsconfig() -> String {
    r#"{
  "compilerOptions": {
    "target": "ES2022",
    "lib": ["ES2022", "DOM", "DOM.Iterable", "DOM.AsyncIterable"],
    "module": "ESNext",
    "moduleDetection": "force",
    "moduleResolution": "bundler",
    "allowImportingTsExtensions": true,
    "strict": true,
    "noEmit": true,
    "skipLibCheck": false,
    "types": []
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
