#![feature(drain_filter)]
#[macro_use]
extern crate anyhow;
#[macro_use]
extern crate serde_derive;

mod changelog;
pub mod commit;
pub mod repository;
mod semver;
pub mod settings;

use crate::changelog::Changelog;
use crate::commit::CommitType;
use crate::repository::Repository;
use crate::settings::Settings;
use anyhow::Result;
use chrono::Utc;
use colored::*;
use commit::Commit;
use git2::{Commit as Git2Commit, Oid, RebaseOptions, Repository as Git2Repository};
use std::fs::File;
use std::io::Write;
use std::process::{Command, Stdio};
use tempdir::TempDir;

pub struct CocoGitto {
    pub settings: Settings,
    repository: Repository,
}

impl CocoGitto {
    pub fn get() -> Result<Self> {
        let repository = Repository::open()?;
        let settings = Settings::get(&repository)?;

        Ok(CocoGitto {
            settings,
            repository,
        })
    }

    pub fn commit_types(&self) -> Vec<String> {
        let mut commit_types: Vec<String> = self
            .settings
            .commit_types
            .iter()
            .map(|(key, _)| key)
            .cloned()
            .collect();

        commit_types.extend_from_slice(&[
            "feat".to_string(),
            "fix".to_string(),
            "chore".to_string(),
            "revert".to_string(),
            "perf".to_string(),
            "docs".to_string(),
            "style".to_string(),
            "refactor".to_string(),
            "test".to_string(),
            "build".to_string(),
            "ci".to_string(),
        ]);

        commit_types
    }

    pub fn get_repository(&self) -> &Git2Repository {
        &self.repository.0
    }

    pub fn check_and_edit(&self) -> Result<()> {
        let from = self.repository.get_first_commit()?;
        let to = self.repository.get_head_commit_oid()?;
        let commits = self.get_commit_range(from, to)?;
        let editor = std::env::var("EDITOR")?;
        let dir = TempDir::new("cocogito")?;

        let errored_commits: Vec<Oid> = commits
            .iter()
            .map(|commit| {
                let conv_commit = Commit::from_git_commit(&commit);
                (commit.id(), conv_commit)
            })
            .filter(|commit| commit.1.is_err())
            .map(|commit| commit.0)
            .collect();

        let commit = self
            .repository
            .0
            .find_commit(errored_commits.last().unwrap().to_owned())?;
        let rebase_start = commit.parent_id(0)?;
        let commit = self.repository.0.find_annotated_commit(rebase_start)?;

        let mut options = RebaseOptions::new();
        let mut rebase = self
            .repository
            .0
            .rebase(None, None, Some(&commit), Some(&mut options))?;

        while let Some(Ok(rebase_operation)) = rebase.next() {
            let oid = rebase_operation.id();
            if errored_commits.contains(&oid) {
                let original_commit = self.repository.0.find_commit(oid)?;
                let file_path = dir.path().join(&commit.id().to_string());
                let mut file = File::create(&file_path)?;
                file.write_all(original_commit.message_bytes())?;

                Command::new(&editor)
                    .arg(&file_path)
                    .stdout(Stdio::inherit())
                    .stdin(Stdio::inherit())
                    .stderr(Stdio::inherit())
                    .output()?;

                let new_message = std::fs::read_to_string(&file_path)?;
                rebase.commit(None, &original_commit.committer(), Some(&new_message))?;
            }
        }

        rebase.finish(None)?;
        Ok(())
    }

    pub fn check(&self) -> Result<()> {
        let from = self.repository.get_first_commit()?;
        let to = self.repository.get_head_commit_oid()?;
        let commits = self.get_commit_range(from, to)?;
        for commit in commits {
            match Commit::from_git_commit(&commit) {
                Ok(_) => (),
                Err(err) => {
                    let err = format!("{}", err).red();
                    eprintln!("{}", err);
                }
            };
        }

        Ok(())
    }

    pub fn verify(message: &str) -> Result<()> {
        Commit::parse_commit_message(message).map(|_| ())
    }

    pub fn conventional_commit(
        &self,
        commit_type: &str,
        scope: Option<String>,
        message: String,
    ) -> Result<()> {
        let commit_type = CommitType::from(commit_type);
        let message = match scope {
            Some(scope) => format!("{}({}): {}", commit_type, scope, message,),
            None => format!("{}: {}", commit_type, message,),
        };

        self.repository.commit(message)
    }

    pub fn publish() -> () {
        todo!()
    }

    pub fn get_changelog(&self, from: Option<&str>, to: Option<&str>) -> anyhow::Result<String> {
        let from = self.resolve_from_arg(from)?;
        let to = self.resolve_to_arg(to)?;

        let mut commits = vec![];

        for commit in self.get_commit_range(from, to)? {
            // We skip the origin commit (ex: from 0.1.0 to 1.0.0)
            if commit.id() == from {
                break;
            }

            match Commit::from_git_commit(&commit) {
                Ok(commit) => commits.push(commit),
                Err(err) => {
                    let err = format!("{}", err).red();
                    eprintln!("{}", err);
                }
            };
        }

        let date = Utc::now().naive_utc().date().to_string();

        let mut changelog = Changelog {
            from,
            to,
            date,
            commits,
        };

        Ok(changelog.tag_diff_to_markdown())
    }

    // TODO : revparse
    fn resolve_to_arg(&self, to: Option<&str>) -> Result<Oid> {
        if let Some(to) = to {
            if to.contains(".") {
                self.repository.resolve_lightweight_tag(to)
            } else {
                Oid::from_str(to).map_err(|err| anyhow!(err))
            }
        } else {
            self.repository
                .get_head_commit_oid()
                .or_else(|_err| self.repository.get_first_commit())
        }
    }

    // TODO : revparse
    fn resolve_from_arg(&self, from: Option<&str>) -> Result<Oid> {
        if let Some(from) = from {
            if from.contains(".") {
                self.repository.resolve_lightweight_tag(from)
            } else {
                Oid::from_str(from).map_err(|err| anyhow!(err))
            }
        } else {
            self.repository
                .get_latest_tag()
                .or_else(|_err| self.repository.get_first_commit())
        }
    }

    fn get_commit_range(&self, from: Oid, to: Oid) -> Result<Vec<Git2Commit>> {
        // Ensure commit exists
        let repository = self.get_repository();
        repository.find_commit(from)?;
        repository.find_commit(to)?;

        let mut revwalk = repository.revwalk()?;
        revwalk.push(to)?;
        revwalk.push(from)?;

        let mut commits: Vec<Git2Commit> = vec![];

        for oid in revwalk {
            let oid = oid?;
            let commit = repository.find_commit(oid)?;
            commits.push(commit);
        }

        Ok(commits)
    }
}

#[cfg(test)]
mod test {
    #[test]
    fn should_open_repo() {}
}
