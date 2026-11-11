#[derive(Clone, Copy)]
pub struct License {
    pub key: &'static str,
    pub name: &'static str,
    pub spdx: &'static str,
    pub id: i64,
}
fn normalized(text: &str) -> String {
    text.chars()
        .map(|ch| {
            if ch.is_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
pub fn license_info(text: &str) -> License {
    let known =
        crate::repositories::templates::detect_license(text.as_bytes()).and_then(|template| {
            let index = crate::repositories::templates::LICENSE_TEMPLATES
                .iter()
                // A const slice has no fixed address, so the template is found by name.
                .position(|known| known.name == template.name)?;
            let (key, name) = match template.name {
                "MIT" => ("mit", "MIT License"),
                "Apache-2.0" => ("apache-2.0", "Apache License 2.0"),
                "GPL-3.0" => ("gpl-3.0", "GNU General Public License v3.0"),
                "BSD-3-Clause" => (
                    "bsd-3-clause",
                    "BSD 3-Clause \"New\" or \"Revised\" License",
                ),
                "MPL-2.0" => ("mpl-2.0", "Mozilla Public License 2.0"),
                "Unlicense" => ("unlicense", "The Unlicense"),
                _ => return None,
            };
            Some(License {
                key,
                name,
                spdx: template.name,
                id: index as i64 + 1,
            })
        });
    known.unwrap_or(License {
        key: "other",
        name: "Other",
        spdx: "NOASSERTION",
        id: 0,
    })
}
fn conduct_text(text: &str) -> String {
    let text = text.replace("\r\n", "\n");
    let body = text.split("## Attribution").next().unwrap_or(&text);
    normalized(
        &body
            .split("\n\n")
            .filter(|part| {
                let text = normalized(part);
                !text.starts_with("contributor covenant code of conduct")
                    && text != "code of conduct"
                    && !text.starts_with("instances of abusive")
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}
pub fn contributor_covenant(text: &str) -> bool {
    let text = conduct_text(text);
    [
        include_str!("templates/conduct/contributor-covenant-14.txt"),
        crate::repositories::templates::CONTRIBUTOR_COVENANT_20,
        include_str!("templates/conduct/contributor-covenant-21.txt"),
        // The text /codes_of_conduct offers.
        crate::repositories::templates::CONTRIBUTOR_COVENANT,
    ]
    .into_iter()
    .any(|template| text == conduct_text(template))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn standard_licenses_survive_the_native_copyright_substitution() {
        for template in crate::repositories::templates::LICENSE_TEMPLATES {
            let filled =
                crate::repositories::templates::fill_license(template.text, 2026, "A person");
            assert_eq!(license_info(&filled).spdx, template.name);
        }
        assert_eq!(license_info("This is not a license").spdx, "NOASSERTION");
    }
    #[test]
    fn covenant_detection_checks_the_policy_not_just_its_heading() {
        assert!(contributor_covenant(include_str!(
            "templates/conduct/contributor-covenant-21.txt"
        )));
        assert!(!contributor_covenant(
            "# Contributor Covenant Code of Conduct\nEveryone may do anything"
        ));
    }
}
