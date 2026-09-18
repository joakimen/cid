//! The commands that install a detected toolchain's dependencies.

use super::Step;
use super::detect::{Detection, PackageManager, PythonMode, Toolchain};

/// An ordered install plan. `mise` runs to completion first because it provides
/// the toolchains the rest need; `parallel` may then run at once, since no
/// package manager's install depends on another's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub mise: Option<Step>,
    pub parallel: Vec<Step>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.mise.is_none() && self.parallel.is_empty()
    }

    /// Every step in the order it is started.
    pub fn steps(&self) -> impl Iterator<Item = &Step> {
        self.mise.iter().chain(self.parallel.iter())
    }

    /// Every step in the order it is started, with the name of the step it
    /// waits for.
    pub fn dependencies(&self) -> impl Iterator<Item = (&Step, Option<&'static str>)> {
        let mise = self.mise.as_ref().map(|step| step.name);
        self.mise
            .iter()
            .map(|step| (step, None))
            .chain(self.parallel.iter().map(move |step| (step, mise)))
    }
}

pub fn plan(detections: &[Detection]) -> Plan {
    let mut mise = None;
    let mut parallel = Vec::new();

    for detection in detections {
        let step = step(detection);
        if detection.toolchain == Toolchain::Mise {
            mise = Some(step);
        } else {
            parallel.push(step);
        }
    }

    Plan { mise, parallel }
}

fn step(detection: &Detection) -> Step {
    let name = detection.toolchain.name();
    let evidence = detection.evidence.clone();
    let step = |program: &str, args: &[&str]| Step::new(name, evidence.clone(), program, args);

    match detection.toolchain {
        Toolchain::Mise => step("mise", &["install"]),
        Toolchain::Rust => step("cargo", &["fetch"]),
        Toolchain::Go => step("go", &["mod", "download"]),
        Toolchain::Node(manager) => match manager {
            PackageManager::Bun => step("bun", &["install"]),
            PackageManager::Pnpm => step("pnpm", &["install"]),
            PackageManager::Yarn => step("yarn", &["install"]),
            PackageManager::Npm => step("npm", &["install"]),
        },
        Toolchain::Deno => step("deno", &["install"]),
        Toolchain::Clojure => step("clojure", &["-P"]),
        Toolchain::Babashka => step("bb", &["prepare"]),
        Toolchain::Maven { wrapper } => step(
            if wrapper { "./mvnw" } else { "mvn" },
            &["-B", "dependency:go-offline"],
        ),
        Toolchain::Gradle { wrapper } => step(
            if wrapper { "./gradlew" } else { "gradle" },
            &["dependencies"],
        ),
        Toolchain::Python(PythonMode::Project) => step("uv", &["sync"]),
        Toolchain::Python(PythonMode::Requirements) => {
            step("uv", &["pip", "install", "-r", "requirements.txt"])
        }
        Toolchain::Terraform => step("terraform", &["init", "-input=false"]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detections(toolchains: &[Toolchain]) -> Vec<Detection> {
        toolchains
            .iter()
            .map(|&toolchain| Detection {
                toolchain,
                evidence: String::new(),
            })
            .collect()
    }

    fn command_for(toolchain: Toolchain) -> String {
        step(&Detection {
            toolchain,
            evidence: String::new(),
        })
        .command_line()
    }

    #[test]
    fn mise_is_held_back_from_the_parallel_steps() {
        let plan = plan(&detections(&[
            Toolchain::Rust,
            Toolchain::Mise,
            Toolchain::Go,
        ]));

        assert_eq!(
            plan.mise.map(|step| step.command_line()),
            Some("mise install".to_string())
        );
        assert_eq!(
            plan.parallel
                .iter()
                .map(Step::command_line)
                .collect::<Vec<_>>(),
            vec!["cargo fetch", "go mod download"]
        );
    }

    #[test]
    fn a_plan_without_mise_runs_everything_in_parallel() {
        let plan = plan(&detections(&[Toolchain::Rust]));

        assert_eq!(plan.mise, None);
        assert_eq!(plan.parallel.len(), 1);
    }

    #[test]
    fn an_empty_toolchain_list_yields_an_empty_plan() {
        assert!(plan(&[]).is_empty());
    }

    #[test]
    fn every_toolchain_installs_with_something() {
        let all = [
            (Toolchain::Mise, "mise install"),
            (Toolchain::Rust, "cargo fetch"),
            (Toolchain::Go, "go mod download"),
            (Toolchain::Node(PackageManager::Bun), "bun install"),
            (Toolchain::Node(PackageManager::Pnpm), "pnpm install"),
            (Toolchain::Node(PackageManager::Yarn), "yarn install"),
            (Toolchain::Node(PackageManager::Npm), "npm install"),
            (Toolchain::Deno, "deno install"),
            (Toolchain::Clojure, "clojure -P"),
            (Toolchain::Babashka, "bb prepare"),
            (
                Toolchain::Maven { wrapper: false },
                "mvn -B dependency:go-offline",
            ),
            (Toolchain::Gradle { wrapper: false }, "gradle dependencies"),
            (Toolchain::Python(PythonMode::Project), "uv sync"),
            (
                Toolchain::Python(PythonMode::Requirements),
                "uv pip install -r requirements.txt",
            ),
            (Toolchain::Terraform, "terraform init -input=false"),
        ];
        for (toolchain, expected) in all {
            assert_eq!(command_for(toolchain), expected, "{toolchain:?}");
        }
    }

    #[test]
    fn a_committed_wrapper_is_preferred_over_the_system_tool() {
        assert_eq!(
            command_for(Toolchain::Maven { wrapper: true }),
            "./mvnw -B dependency:go-offline"
        );
        assert_eq!(
            command_for(Toolchain::Gradle { wrapper: true }),
            "./gradlew dependencies"
        );
    }

    #[test]
    fn steps_carry_the_evidence_that_selected_them() {
        let plan = plan(&[Detection {
            toolchain: Toolchain::Node(PackageManager::Bun),
            evidence: "package.json + bun.lock".to_string(),
        }]);

        assert_eq!(plan.parallel[0].evidence, "package.json + bun.lock");
    }

    #[test]
    fn every_other_step_waits_for_mise() {
        let plan = plan(&detections(&[
            Toolchain::Rust,
            Toolchain::Mise,
            Toolchain::Go,
        ]));

        assert_eq!(
            plan.dependencies()
                .map(|(step, after)| (step.name, after))
                .collect::<Vec<_>>(),
            vec![("mise", None), ("rust", Some("mise")), ("go", Some("mise"))]
        );
    }

    #[test]
    fn without_mise_no_step_waits() {
        let plan = plan(&detections(&[Toolchain::Rust, Toolchain::Go]));

        assert!(plan.dependencies().all(|(_, after)| after.is_none()));
    }

    #[test]
    fn the_steps_are_listed_with_mise_first() {
        let plan = plan(&detections(&[Toolchain::Rust, Toolchain::Mise]));

        assert_eq!(
            plan.steps().map(|step| step.name).collect::<Vec<_>>(),
            vec!["mise", "rust"]
        );
    }
}
