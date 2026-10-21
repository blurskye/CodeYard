//! Templates for the files a new repository can start with: .gitignore files and licenses.
//! The new repository form offers the names listed in `webui::pages::discovery`. A test keeps
//! those lists and the templates here in step.

/// A named template. `name` is what the form submits.
#[derive(Debug, PartialEq, Eq)]
pub struct Template {
    pub name: &'static str,
    pub text: &'static str,
}

pub const GITIGNORE_TEMPLATES: &[Template] = &[
    Template {
        name: "Rust",
        text: RUST_GITIGNORE,
    },
    Template {
        name: "Node",
        text: NODE_GITIGNORE,
    },
    Template {
        name: "Python",
        text: PYTHON_GITIGNORE,
    },
    Template {
        name: "Go",
        text: GO_GITIGNORE,
    },
    Template {
        name: "Java",
        text: JAVA_GITIGNORE,
    },
    Template {
        name: "C",
        text: C_GITIGNORE,
    },
    Template {
        name: "C++",
        text: CPP_GITIGNORE,
    },
    Template {
        name: "Dotnet",
        text: DOTNET_GITIGNORE,
    },
    Template {
        name: "Ruby",
        text: RUBY_GITIGNORE,
    },
    Template {
        name: "Swift",
        text: SWIFT_GITIGNORE,
    },
    Template {
        name: "Kotlin",
        text: KOTLIN_GITIGNORE,
    },
];

pub const LICENSE_TEMPLATES: &[Template] = &[
    Template {
        name: "MIT",
        text: MIT_LICENSE,
    },
    Template {
        name: "Apache-2.0",
        text: APACHE_2_0_LICENSE,
    },
    Template {
        name: "GPL-3.0",
        text: GPL_3_0_LICENSE,
    },
    Template {
        name: "BSD-3-Clause",
        text: BSD_3_CLAUSE_LICENSE,
    },
    Template {
        name: "MPL-2.0",
        text: MPL_2_0_LICENSE,
    },
    Template {
        name: "Unlicense",
        text: UNLICENSE_LICENSE,
    },
];

pub fn gitignore(name: &str) -> Option<&'static Template> {
    GITIGNORE_TEMPLATES
        .iter()
        .find(|template| template.name == name)
}

pub fn license(name: &str) -> Option<&'static Template> {
    LICENSE_TEMPLATES
        .iter()
        .find(|template| template.name == name)
}

/// The README a new repository starts with: its name, then its description when it has one.
pub fn readme(name: &str, description: Option<&str>) -> String {
    match description {
        Some(description) => format!("# {name}\n\n{description}\n"),
        None => format!("# {name}\n"),
    }
}

/// Puts the copyright holder and the year into a license. Licenses without a copyright line
/// (GPL, MPL and Unlicense) come back unchanged.
pub fn fill_license(text: &str, year: i32, holder: &str) -> String {
    let year = year.to_string();
    // The holder goes in last and only into the template's own text, so a name that looks
    // like a placeholder stays as typed.
    text.replace("[year]", &year)
        .replace("[yyyy]", &year)
        .split("[fullname]")
        .map(|part| {
            part.split("[name of copyright owner]")
                .collect::<Vec<_>>()
                .join(holder)
        })
        .collect::<Vec<_>>()
        .join(holder)
}

const RUST_GITIGNORE: &str = include_str!("templates/gitignore/rust.txt");
const NODE_GITIGNORE: &str = include_str!("templates/gitignore/node.txt");
const PYTHON_GITIGNORE: &str = include_str!("templates/gitignore/python.txt");
const GO_GITIGNORE: &str = include_str!("templates/gitignore/go.txt");
const JAVA_GITIGNORE: &str = include_str!("templates/gitignore/java.txt");
const C_GITIGNORE: &str = include_str!("templates/gitignore/c.txt");
const CPP_GITIGNORE: &str = include_str!("templates/gitignore/cpp.txt");
const DOTNET_GITIGNORE: &str = include_str!("templates/gitignore/dotnet.txt");
const RUBY_GITIGNORE: &str = include_str!("templates/gitignore/ruby.txt");
const SWIFT_GITIGNORE: &str = include_str!("templates/gitignore/swift.txt");
const KOTLIN_GITIGNORE: &str = include_str!("templates/gitignore/kotlin.txt");
const MIT_LICENSE: &str = include_str!("templates/license/mit.txt");
const APACHE_2_0_LICENSE: &str = include_str!("templates/license/apache-2-0.txt");
const GPL_3_0_LICENSE: &str = include_str!("templates/license/gpl-3-0.txt");
const BSD_3_CLAUSE_LICENSE: &str = include_str!("templates/license/bsd-3-clause.txt");
const MPL_2_0_LICENSE: &str = include_str!("templates/license/mpl-2-0.txt");
const UNLICENSE_LICENSE: &str = include_str!("templates/license/unlicense.txt");

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webui::pages::discovery::{GITIGNORE_CHOICES, LICENSE_CHOICES};

    #[test]
    fn templates_ignore_local_secrets_but_keep_gradle_wrappers() {
        let root = tempfile::tempdir().unwrap();
        git2::Repository::init(root.path()).unwrap();
        for (template, path, ignored) in [
            (NODE_GITIGNORE, ".env.local", true),
            (NODE_GITIGNORE, ".env.production.local", true),
            (JAVA_GITIGNORE, "gradle/wrapper/gradle-wrapper.jar", false),
            (KOTLIN_GITIGNORE, "gradle/wrapper/gradle-wrapper.jar", false),
            (JAVA_GITIGNORE, "library.jar", true),
            (KOTLIN_GITIGNORE, "library.jar", true),
        ] {
            std::fs::write(root.path().join(".gitignore"), template).unwrap();
            let repo = git2::Repository::open(root.path()).unwrap();
            assert_eq!(
                repo.status_should_ignore(std::path::Path::new(path))
                    .unwrap(),
                ignored,
                "{path}"
            );
        }
    }

    #[test]
    fn every_template_is_offered_and_every_offered_name_has_a_template() {
        let gitignore: Vec<&str> = GITIGNORE_TEMPLATES
            .iter()
            .map(|template| template.name)
            .collect();
        let licenses: Vec<&str> = LICENSE_TEMPLATES
            .iter()
            .map(|template| template.name)
            .collect();
        assert_eq!(gitignore, GITIGNORE_CHOICES);
        assert_eq!(licenses, LICENSE_CHOICES);
    }
}
