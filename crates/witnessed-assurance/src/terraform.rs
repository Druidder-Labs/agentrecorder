use std::collections::BTreeSet;

pub fn block_types(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in text.lines() {
        let line = line.trim_start();
        let (kind, rest) = if let Some(rest) = line.strip_prefix("resource") {
            ("", rest)
        } else if let Some(rest) = line.strip_prefix("data") {
            ("data.", rest)
        } else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('"') else { continue };
        let Some(end) = rest.find('"') else { continue };
        let name = &rest[..end];
        if !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
            out.insert(format!("{kind}{name}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_and_data_block_types_are_listed_once() {
        let tf = r#"
resource "aws_iam_role_policy" "deploy" {
  policy = data.aws_iam_policy_document.deploy.json
}
  data "aws_iam_policy_document" "deploy" {}
resource "aws_s3_bucket" "logs" {}
resource "aws_iam_role_policy" "other" {}
# resource "aws_iam_user" "commented" {}
resourcefoo "x" {}
"#;
        let types: Vec<String> = block_types(tf).into_iter().collect();
        assert_eq!(types, ["aws_iam_role_policy", "aws_s3_bucket", "data.aws_iam_policy_document"]);
    }
}
