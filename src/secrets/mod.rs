//! Secrets local resolver.
//!
//! Resolves ECS Secrets Manager ARN references to local plaintext values
//! using a mapping file (`secrets.local.json`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::taskdef::Secret;

/// Secrets resolution errors.
#[derive(Debug, thiserror::Error)]
pub enum SecretsError {
    #[error("failed to read secrets file from {path}: {source}")]
    ReadFile {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to parse secrets JSON: {0}")]
    ParseJson(#[from] serde_json::Error),

    #[error("secret ARN not found in local mapping: {arn}")]
    ArnNotFound { arn: String },
}

/// Resolves Secrets Manager ARN references to local values.
#[derive(Debug)]
pub struct SecretsResolver {
    mapping: HashMap<String, String>,
}

impl SecretsResolver {
    /// Load a secrets mapping from a file path.
    pub fn from_file(path: &Path) -> Result<Self, SecretsError> {
        let content = std::fs::read_to_string(path).map_err(|source| SecretsError::ReadFile {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_json(&content)
    }

    /// Parse a secrets mapping from a JSON string.
    pub fn from_json(json: &str) -> Result<Self, SecretsError> {
        let mapping: HashMap<String, String> = serde_json::from_str(json)?;
        Ok(Self { mapping })
    }

    /// Resolve a list of secrets to `(name, value)` pairs.
    ///
    /// Returns an error if any ARN is not found in the mapping.
    pub fn resolve(&self, secrets: &[Secret]) -> Result<Vec<(String, String)>, SecretsError> {
        secrets
            .iter()
            .map(|secret| {
                let value = self.mapping.get(&secret.value_from).ok_or_else(|| {
                    SecretsError::ArnNotFound {
                        arn: secret.value_from.clone(),
                    }
                })?;
                Ok((secret.name.clone(), value.clone()))
            })
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::similar_names)]
mod tests {
    use super::*;

    #[test]
    fn parse_secrets_mapping() {
        let json = r#"{
            "arn:aws:secretsmanager:us-east-1:123:secret:db-pass": "local-db-password",
            "arn:aws:secretsmanager:us-east-1:123:secret:api-key": "local-api-key"
        }"#;
        let resolver = SecretsResolver::from_json(json).expect("should parse");
        assert_eq!(resolver.mapping.len(), 2);
    }

    #[test]
    fn resolve_all_found() {
        let json = r#"{
            "arn:aws:secretsmanager:us-east-1:123:secret:db-pass": "local-db-password",
            "arn:aws:secretsmanager:us-east-1:123:secret:api-key": "local-api-key"
        }"#;
        let resolver = SecretsResolver::from_json(json).expect("should parse");

        let secrets = vec![
            Secret {
                name: "DB_PASSWORD".to_string(),
                value_from: "arn:aws:secretsmanager:us-east-1:123:secret:db-pass".to_string(),
            },
            Secret {
                name: "API_KEY".to_string(),
                value_from: "arn:aws:secretsmanager:us-east-1:123:secret:api-key".to_string(),
            },
        ];

        let resolved = resolver.resolve(&secrets).expect("should resolve");
        assert_eq!(resolved.len(), 2);
        assert_eq!(
            resolved[0],
            ("DB_PASSWORD".to_string(), "local-db-password".to_string())
        );
        assert_eq!(
            resolved[1],
            ("API_KEY".to_string(), "local-api-key".to_string())
        );
    }

    #[test]
    fn resolve_missing_arn() {
        let json = r#"{
            "arn:aws:secretsmanager:us-east-1:123:secret:db-pass": "local-db-password"
        }"#;
        let resolver = SecretsResolver::from_json(json).expect("should parse");

        let secrets = vec![Secret {
            name: "MISSING".to_string(),
            value_from: "arn:aws:secretsmanager:us-east-1:123:secret:unknown".to_string(),
        }];

        let err = resolver.resolve(&secrets).unwrap_err();
        assert!(
            matches!(err, SecretsError::ArnNotFound { ref arn } if arn.contains("unknown")),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_empty_secrets() {
        let resolver = SecretsResolver::from_json("{}").expect("should parse");
        let resolved = resolver.resolve(&[]).expect("should resolve");
        assert!(resolved.is_empty());
    }

    #[test]
    fn error_invalid_json() {
        let err = SecretsResolver::from_json("not json").unwrap_err();
        assert!(matches!(err, SecretsError::ParseJson(_)));
    }

    #[test]
    fn error_file_not_found() {
        let err = SecretsResolver::from_file(Path::new("/nonexistent/secrets.json")).unwrap_err();
        assert!(
            matches!(err, SecretsError::ReadFile { .. }),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn error_display_messages() {
        let err = SecretsResolver::from_json("bad").unwrap_err();
        assert!(err.to_string().contains("failed to parse secrets JSON"));

        let err = SecretsResolver::from_file(Path::new("/no/such")).unwrap_err();
        assert!(err.to_string().contains("failed to read secrets file"));

        let err = SecretsError::ArnNotFound {
            arn: "arn:test".to_string(),
        };
        assert!(err.to_string().contains("arn:test"));
    }

    // --- Property-based tests ---

    mod pbt {
        use super::*;
        use proptest::prelude::*;

        fn arb_arn() -> impl Strategy<Value = String> {
            ("[a-z0-9-]{1,20}", "[0-9]{12}", "[a-zA-Z0-9/_+=.@-]{1,20}").prop_map(
                |(region, account, name)| {
                    format!("arn:aws:secretsmanager:{region}:{account}:secret:{name}")
                },
            )
        }

        fn arb_secret_name() -> impl Strategy<Value = String> {
            "[A-Z][A-Z0-9_]{0,15}"
        }

        fn arb_secret_value() -> impl Strategy<Value = String> {
            "[a-zA-Z0-9_.-]{1,30}"
        }

        /// Generate (mapping, secrets) where every secret references an ARN in the mapping.
        fn arb_mapping_and_secrets(
            n: usize,
        ) -> impl Strategy<Value = (HashMap<String, String>, Vec<Secret>)> {
            let pairs_strat = proptest::collection::vec(
                (arb_arn(), arb_secret_value(), arb_secret_name()),
                n..=n,
            );
            pairs_strat.prop_map(|triples| {
                // Deduplicate ARNs, keeping the first occurrence.
                let mut mapping = HashMap::new();
                let mut secrets = Vec::new();
                for (arn, value, name) in triples {
                    mapping.entry(arn.clone()).or_insert_with(|| value.clone());
                    secrets.push(Secret {
                        name,
                        value_from: arn,
                    });
                }
                (mapping, secrets)
            })
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(300))]

            /// Property: When every secret ARN is in the mapping, resolve succeeds.
            #[test]
            fn all_present_resolves_ok((mapping, secrets) in arb_mapping_and_secrets(3)) {
                let resolver = SecretsResolver { mapping };
                let result = resolver.resolve(&secrets);
                prop_assert!(result.is_ok(), "expected Ok, got {:?}", result.err());
            }

            /// Property: resolved list preserves order and count of input secrets.
            #[test]
            fn resolve_preserves_order_and_count(
                (mapping, secrets) in arb_mapping_and_secrets(4),
            ) {
                let resolver = SecretsResolver { mapping };
                let resolved = resolver.resolve(&secrets).expect("resolves");
                prop_assert_eq!(resolved.len(), secrets.len());
                for (i, (name, _)) in resolved.iter().enumerate() {
                    prop_assert_eq!(name, &secrets[i].name);
                }
            }

            /// Property: resolved values equal the mapping values for the same ARN.
            #[test]
            fn resolve_values_match_mapping(
                (mapping, secrets) in arb_mapping_and_secrets(4),
            ) {
                let resolver = SecretsResolver { mapping: mapping.clone() };
                let resolved = resolver.resolve(&secrets).expect("resolves");
                for (i, (_name, value)) in resolved.iter().enumerate() {
                    let expected = mapping
                        .get(&secrets[i].value_from)
                        .expect("value exists");
                    prop_assert_eq!(value, expected);
                }
            }

            /// Property: A missing ARN always produces ArnNotFound with that ARN.
            #[test]
            fn missing_arn_produces_arn_not_found(
                (mapping, _) in arb_mapping_and_secrets(2),
                missing_arn in arb_arn(),
                secret_name in arb_secret_name(),
            ) {
                // Ensure the generated arn is not in the mapping.
                let missing = if mapping.contains_key(&missing_arn) {
                    format!("{missing_arn}xx-not-present")
                } else {
                    missing_arn
                };
                let resolver = SecretsResolver { mapping };
                let secrets = vec![Secret {
                    name: secret_name,
                    value_from: missing.clone(),
                }];
                let err = resolver.resolve(&secrets).expect_err("should be missing");
                let is_not_found = matches!(
                    err,
                    SecretsError::ArnNotFound { ref arn } if arn == &missing
                );
                prop_assert!(is_not_found, "unexpected error: {err}");
            }

            /// Property: resolving an empty slice always yields Ok([]).
            #[test]
            fn empty_secrets_always_ok((mapping, _) in arb_mapping_and_secrets(2)) {
                let resolver = SecretsResolver { mapping };
                let resolved = resolver.resolve(&[]).expect("resolves");
                prop_assert!(resolved.is_empty());
            }

            /// Property: Serializing a mapping as JSON and parsing it back yields the
            /// same mapping (lookup-equivalent).
            #[test]
            fn json_roundtrip_parses((mapping, _) in arb_mapping_and_secrets(3)) {
                let json = serde_json::to_string(&mapping).expect("serializes");
                let roundtrip = SecretsResolver::from_json(&json).expect("parses");
                for (k, v) in &mapping {
                    prop_assert_eq!(roundtrip.mapping.get(k), Some(v));
                }
                prop_assert_eq!(roundtrip.mapping.len(), mapping.len());
            }
        }
    }
}
