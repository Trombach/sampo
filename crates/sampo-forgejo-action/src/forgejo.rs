use crate::error::{ActionError, Result};
use reqwest::{Url, blocking::Client};
use serde::{Deserialize, Serialize};
use std::{path::Path, time::Duration};

#[derive(Debug, Serialize)]
struct CreatePullRequestPayload {
    title: String,
    head: String,
    base: String,
    body: String,
}

#[derive(Debug, Deserialize)]
struct PullRequest {
    number: u64,
    html_url: String,
}

#[derive(Debug, Serialize)]
struct UpdatePullRequestPayload {
    title: String,
    body: String,
}

#[derive(Debug, Serialize)]
struct CreateReleasePayload {
    tag_name: String,
    name: String,
    body: String,
    draft: bool,
    prerelease: bool,
}

#[derive(Debug, Deserialize)]
struct Release {
    #[serde(rename = "upload_url")]
    upload_url: String,
    html_url: String,
}

pub struct ForgejoClient {
    client: Client,
    base_url: String,
    repo: String,
    token: String,
}

impl ForgejoClient {
    pub fn new(base_url: String, repo: String, token: String) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(format!(
                "sampo-forgejo-action/{}",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .map_err(|e| ActionError::SampoCommandFailed {
                operation: "http-client-init".to_string(),
                message: format!("Failed to create HTTP client: {}", e),
            })?;

        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            repo,
            token,
        })
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.token)
    }

    /// Create or update a Forgejo Pull Request
    pub fn ensure_pull_request(
        &self,
        head_branch: &str,
        base_branch: &str,
        title: &str,
        body: &str,
    ) -> Result<()> {
        let api_url = format!("{}/repos/{}/pulls", self.base_url, self.repo);

        println!("Creating/updating PR: {} <- {}", base_branch, head_branch);

        let payload = CreatePullRequestPayload {
            title: title.to_string(),
            head: head_branch.to_string(),
            base: base_branch.to_string(),
            body: body.to_string(),
        };

        // Try to create the PR
        let response = self
            .client
            .post(&api_url)
            .header("Authorization", self.auth_header())
            .header("Accept", "application/json")
            .json(&payload)
            .send()
            .map_err(|e| ActionError::SampoCommandFailed {
                operation: "forgejo-create-pr".to_string(),
                message: format!("HTTP request to {} failed: {}", api_url, e),
            })?;

        if response.status().is_success() {
            let pr: PullRequest = response
                .json()
                .map_err(|e| ActionError::SampoCommandFailed {
                    operation: "forgejo-create-pr".to_string(),
                    message: format!("Failed to parse successful PR response: {}", e),
                })?;

            println!("PR created successfully: {}", pr.html_url);
            return Ok(());
        }

        // Handle error responses
        let status = response.status();
        let error_text = response
            .text()
            .map_err(|e| ActionError::SampoCommandFailed {
                operation: "forgejo-create-pr".to_string(),
                message: format!("Failed to read error response body: {}", e),
            })?;

        // Check if PR already exists
        if status == 422 && error_text.contains("A pull request already exists") {
            println!("PR already exists, attempting to update...");
            return self.find_and_update_existing_pr(head_branch, title, body);
        }

        // Return the Forgejo API error with context
        Err(ActionError::SampoCommandFailed {
            operation: "forgejo-create-pr".to_string(),
            message: format!(
                "Forgejo API error for {}: {} (status {})",
                self.repo, error_text, status
            ),
        })
    }

    fn find_and_update_existing_pr(
        &self,
        head_branch: &str,
        title: &str,
        body: &str,
    ) -> Result<()> {
        let owner = self.repo.split('/').next().unwrap_or("");
        let list_url = format!(
            "{}/repos/{}/pulls?state=open&head={}:{}",
            self.base_url, self.repo, owner, head_branch
        );

        let response = self
            .client
            .get(&list_url)
            .header("Authorization", self.auth_header())
            .header("Accept", "application/json")
            .send()
            .map_err(|e| ActionError::SampoCommandFailed {
                operation: "forgejo-list-prs".to_string(),
                message: format!("HTTP request to {} failed: {}", list_url, e),
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().unwrap_or_default();
            return Err(ActionError::SampoCommandFailed {
                operation: "forgejo-list-prs".to_string(),
                message: format!("Failed to list PRs ({}): {}", status, error_text),
            });
        }

        let prs: Vec<PullRequest> =
            response
                .json()
                .map_err(|e| ActionError::SampoCommandFailed {
                    operation: "forgejo-list-prs".to_string(),
                    message: format!("Failed to parse PR list response: {}", e),
                })?;

        if let Some(pr) = prs.first() {
            println!("Found existing PR #{}, updating...", pr.number);
            self.update_pull_request(pr.number, title, body)
        } else {
            println!("No open PR found for {}:{}", self.repo, head_branch);
            Ok(())
        }
    }

    /// Update and exising Pull Request
    fn update_pull_request(&self, pr_number: u64, title: &str, body: &str) -> Result<()> {
        let api_url = format!("{}/repos/{}/pulls/{}", self.base_url, self.repo, pr_number);

        let payload = UpdatePullRequestPayload {
            title: title.to_string(),
            body: body.to_string(),
        };

        let response = self
            .client
            .patch(&api_url)
            .header("Authorization", self.auth_header())
            .header("Accept", "application/json")
            .json(&payload)
            .send()
            .map_err(|e| ActionError::SampoCommandFailed {
                operation: "forgejo-update-pr".to_string(),
                message: format!("HTTP request failed: {}", e),
            })?;

        if response.status().is_success() {
            let pr: PullRequest = response
                .json()
                .map_err(|e| ActionError::SampoCommandFailed {
                    operation: "forgejo-update-pr".to_string(),
                    message: format!("Failed to parse PR response: {}", e),
                })?;

            println!("PR updated successfully: {}", pr.html_url);
            Ok(())
        } else {
            let status = response.status();
            let error_text = response.text().unwrap_or_default();
            Err(ActionError::SampoCommandFailed {
                operation: "forgejo-update-pr".to_string(),
                message: format!(
                    "Failed to update PR #{} ({}): {}",
                    pr_number, status, error_text
                ),
            })
        }
    }

    /// Create a Forgejo Release for a tag.
    pub fn create_release(&self, tag: &str, body: &str, prerelease: bool) -> Result<String> {
        let api_url = format!("{}/repos/{}/releases", self.base_url, self.repo);

        let payload = CreateReleasePayload {
            tag_name: tag.to_string(),
            name: tag.to_string(),
            body: body.to_string(),
            draft: false,
            prerelease,
        };

        let response = self
            .client
            .post(&api_url)
            .header("Authorization", self.auth_header())
            .header("Accept", "application/json")
            .json(&payload)
            .send()
            .map_err(|e| ActionError::SampoCommandFailed {
                operation: "forgejo-create-release".to_string(),
                message: format!("HTTP request failed: {}", e),
            })?;

        if response.status().is_success() {
            let release: Release =
                response
                    .json()
                    .map_err(|e| ActionError::SampoCommandFailed {
                        operation: "forgejo-create-release".to_string(),
                        message: format!("Failed to parse release response: {}", e),
                    })?;

            println!("Created Forgejo release fro {}: {}", tag, release.html_url);

            let upload_url = release
                .upload_url
                .split('{')
                .next()
                .unwrap_or("")
                .to_string();
            Ok(upload_url)
        } else {
            let status = response.status();
            let error_text = response.text().unwrap_or_default();

            // If the release already exists, fetch it and return its upload URL
            if status == 422 && error_text.contains("already_exists") {
                eprintln!(
                    "Warning: Release for {} already exists, fetchign existing release.",
                    tag
                );
                return self.get_release_by_tag(tag);
            }

            Err(ActionError::SampoCommandFailed {
                operation: "forgejo-create-release".to_string(),
                message: format!("Failed to create release ({}): {}", status, error_text),
            })
        }
    }

    /// Fetch an existing Forgejo Release by tag name.
    pub fn get_release_by_tag(&self, tag: &str) -> Result<String> {
        let api_url = format!(
            "{}/repos/{}/releases/tags/{}",
            self.base_url, self.repo, tag
        );

        let response = self
            .client
            .get(&api_url)
            .header("Authorization", self.auth_header())
            .header("Accept", "application/json")
            .send()
            .map_err(|e| ActionError::SampoCommandFailed {
                operation: "forgejo-get-release".to_string(),
                message: format!("HTTP request failed: {}", e),
            })?;

        if response.status().is_success() {
            let release: Release =
                response
                    .json()
                    .map_err(|e| ActionError::SampoCommandFailed {
                        operation: "forgejo-get-release".to_string(),
                        message: format!("Failed to parse release response: {}", e),
                    })?;

            println!(
                "Found existing Forgejo release for {}: {}",
                tag, release.html_url
            );

            let upload_url = release
                .upload_url
                .split('{')
                .next()
                .unwrap_or("")
                .to_string();
            Ok(upload_url)
        } else {
            let status = response.status();
            let error_text = response.text().unwrap_or_default();
            Err(ActionError::SampoCommandFailed {
                operation: "forgejo-get-release".to_string(),
                message: format!(
                    "Failed to fetch release for tag {} ({}): {}",
                    tag, status, error_text
                ),
            })
        }
    }

    /// Upload an existing file as a release asset
    pub fn upload_release_asset(
        &self,
        upload_url: &str,
        asset_path: &Path,
        asset_name: &str,
    ) -> Result<()> {
        if !asset_path.is_file() {
            return Err(ActionError::SampoCommandFailed {
                operation: "release-asset-upload".to_string(),
                message: format!(
                    "Release asset not found or not a file: {}",
                    asset_path.display()
                ),
            });
        }

        let asset_bytes = std::fs::read(asset_path).map_err(ActionError::Io)?;

        let mut url = Url::parse(upload_url).map_err(|e| ActionError::SampoCommandFailed {
            operation: "release-asset-upload".to_string(),
            message: format!("Invalid upload URL '{}': {}", upload_url, e),
        })?;
        url.query_pairs_mut().append_pair("name", asset_name);

        let response = self
            .client
            .post(url)
            .header("Authorization", self.auth_header())
            .header("Accept", "application/json")
            .header("Content-Type", "application/octet-stream")
            .body(asset_bytes)
            .send()
            .map_err(|e| ActionError::SampoCommandFailed {
                operation: "release-asset-upload".to_string(),
                message: format!("HTTP request failed: {}", e),
            })?;

        if response.status().is_success() {
            Ok(())
        } else {
            let status = response.status();
            let error_text = response.text().unwrap_or_default();
            Err(ActionError::SampoCommandFailed {
                operation: "release-asset-upload".to_string(),
                message: format!("Failed to upload asset ({}): {}", status, error_text),
            })
        }
    }
}
