use std::collections::BTreeMap;

pub fn substitute(template: &str, vars: &BTreeMap<String, String>) -> String {
    let mut result = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '$' {
            if chars.peek() == Some(&'{') {
                chars.next();
                let mut name = String::new();
                let mut modifier = None;
                let mut mod_value = String::new();

                while let Some(&c) = chars.peek() {
                    if c == '}' {
                        chars.next();
                        break;
                    }
                    if c == ':' && modifier.is_none() {
                        chars.next();
                        if let Some(&next) = chars.peek() {
                            modifier = Some(next);
                            chars.next();
                            while let Some(&c) = chars.peek() {
                                if c == '}' {
                                    chars.next();
                                    break;
                                }
                                mod_value.push(c);
                                chars.next();
                            }
                            break;
                        }
                    }
                    name.push(c);
                    chars.next();
                }

                let resolved = match modifier {
                    Some('-') => vars
                        .get(&name)
                        .filter(|v| !v.is_empty())
                        .cloned()
                        .unwrap_or(mod_value),
                    Some('+') => {
                        if vars.get(&name).is_some_and(|v| !v.is_empty()) {
                            mod_value
                        } else {
                            String::new()
                        }
                    }
                    _ => vars.get(&name).cloned().unwrap_or_default(),
                };
                result.push_str(&resolved);
            } else {
                let mut name = String::new();
                while chars
                    .peek()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
                {
                    name.push(chars.next().unwrap());
                }
                if name.is_empty() {
                    result.push('$');
                } else {
                    result.push_str(vars.get(&name).map(String::as_str).unwrap_or(""));
                }
            }
        } else {
            result.push(ch);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn simple_substitution() {
        let v = vars(&[("NAME", "world")]);
        assert_eq!(substitute("hello $NAME", &v), "hello world");
    }

    #[test]
    fn braced_substitution() {
        let v = vars(&[("NAME", "world")]);
        assert_eq!(substitute("hello ${NAME}", &v), "hello world");
    }

    #[test]
    fn default_value() {
        let v = vars(&[]);
        assert_eq!(substitute("${NAME:-default}", &v), "default");
        let v = vars(&[("NAME", "actual")]);
        assert_eq!(substitute("${NAME:-default}", &v), "actual");
    }

    #[test]
    fn alternate_value() {
        let v = vars(&[("NAME", "set")]);
        assert_eq!(substitute("${NAME:+alt}", &v), "alt");
        let v = vars(&[]);
        assert_eq!(substitute("${NAME:+alt}", &v), "");
    }

    #[test]
    fn missing_var_produces_empty() {
        let v = vars(&[]);
        assert_eq!(substitute("hello $MISSING end", &v), "hello  end");
    }

    #[test]
    fn dollar_without_var() {
        let v = vars(&[]);
        assert_eq!(substitute("price is $", &v), "price is $");
    }

    #[test]
    fn multiple_vars() {
        let v = vars(&[("A", "1"), ("B", "2")]);
        assert_eq!(substitute("$A and ${B}", &v), "1 and 2");
    }
}
