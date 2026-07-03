use std::collections::BTreeMap;

/// env 값. secret은 평문 대신 credentials.id만 참조한다 (설계문서 6.3).
/// 평문 해석은 spawn 직전(PR-09)에만 일어난다.
#[derive(Debug, Clone, PartialEq)]
pub enum EnvValue {
    Plain(String),
    Secret { credential_id: String },
}

/// precedence 한 계층 (설계문서 6.2). resolve에는 낮음 → 높음 순으로 전달한다.
pub struct EnvLayer {
    pub name: String,
    pub vars: Vec<(String, EnvValue)>,
}

/// resolve 결과 한 항목. 충돌 시 UI 표시용으로 가려진 계층을 남긴다 (6.2).
#[derive(Debug, PartialEq)]
pub struct ResolvedVar {
    pub key: String,
    pub value: EnvValue,
    pub source: String,
    /// 이 키를 정의했지만 더 높은 계층에 가려진 layer 이름들 (낮은 순)
    pub overridden: Vec<String>,
}

/// EnvPrecedenceResolver: 마지막(높은) 계층이 이긴다. key 오름차순 반환.
pub fn resolve(layers: &[EnvLayer]) -> Vec<ResolvedVar> {
    let mut map: BTreeMap<String, ResolvedVar> = BTreeMap::new();
    for layer in layers {
        for (key, value) in &layer.vars {
            match map.get_mut(key) {
                Some(existing) => {
                    let prev = std::mem::replace(&mut existing.source, layer.name.clone());
                    existing.overridden.push(prev);
                    existing.value = value.clone();
                }
                None => {
                    map.insert(
                        key.clone(),
                        ResolvedVar {
                            key: key.clone(),
                            value: value.clone(),
                            source: layer.name.clone(),
                            overridden: Vec::new(),
                        },
                    );
                }
            }
        }
    }
    map.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(name: &str, vars: &[(&str, &str)]) -> EnvLayer {
        EnvLayer {
            name: name.into(),
            vars: vars
                .iter()
                .map(|(k, v)| ((*k).into(), EnvValue::Plain((*v).into())))
                .collect(),
        }
    }

    #[test]
    fn 높은_계층이_이긴다() {
        let resolved = resolve(&[
            layer("os", &[("KEY", "os-value")]),
            layer("profile", &[("KEY", "profile-value")]),
        ]);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].value, EnvValue::Plain("profile-value".into()));
        assert_eq!(resolved[0].source, "profile");
        assert_eq!(resolved[0].overridden, vec!["os".to_owned()]);
    }

    #[test]
    fn 충돌_없는_키는_그대로() {
        let resolved = resolve(&[layer("os", &[("A", "1")]), layer("profile", &[("B", "2")])]);
        assert_eq!(resolved.len(), 2);
        assert!(resolved.iter().all(|v| v.overridden.is_empty()));
    }

    #[test]
    fn secret은_credential_id로_유지된다() {
        let profile = EnvLayer {
            name: "profile".into(),
            vars: vec![(
                "API_KEY".into(),
                EnvValue::Secret {
                    credential_id: "cred-1".into(),
                },
            )],
        };
        let resolved = resolve(&[profile]);
        assert_eq!(
            resolved[0].value,
            EnvValue::Secret {
                credential_id: "cred-1".into()
            }
        );
    }

    #[test]
    fn 삼중_충돌시_가려진_계층이_순서대로_남는다() {
        let resolved = resolve(&[
            layer("os", &[("K", "1")]),
            layer("workspace", &[("K", "2")]),
            layer("profile", &[("K", "3")]),
        ]);
        assert_eq!(resolved[0].source, "profile");
        assert_eq!(
            resolved[0].overridden,
            vec!["os".to_owned(), "workspace".to_owned()]
        );
    }
}
