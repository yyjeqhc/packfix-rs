use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tracing::warn;

#[derive(Debug, Clone)]
pub struct ObsCredentials {
    pub user: String,
    pub pass: String,
}

pub fn read_osc_credentials(oscrc_path: &Path) -> Result<ObsCredentials> {
    let content = std::fs::read_to_string(oscrc_path)
        .with_context(|| format!("failed to read oscrc: {}", oscrc_path.display()))?;
    // Be permissive: accept user= and pass= anywhere in the file (multiple sections may exist).
    let mut user = None;
    let mut pass = None;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if user.is_none()
            && let Some(val) = trimmed.strip_prefix("user=")
        {
            user = Some(val.trim().to_string());
        }
        if pass.is_none()
            && let Some(val) = trimmed.strip_prefix("pass=")
        {
            pass = Some(val.trim().to_string());
        }
        if user.is_some() && pass.is_some() {
            break;
        }
    }

    let user = user.unwrap_or_default();
    let pass = pass.unwrap_or_default();
    if user.is_empty() || pass.is_empty() {
        let exists = oscrc_path.exists();
        bail!(
            "could not find user= / pass= in oscrc '{}' (file exists: {exists})",
            oscrc_path.display()
        );
    }
    Ok(ObsCredentials { user, pass })
}

pub async fn obs_get(
    client: &reqwest::Client,
    api_url: &str,
    path: &str,
    creds: &ObsCredentials,
) -> Result<(u16, String)> {
    let url = format!("{api_url}{path}");
    let resp = client
        .get(&url)
        .basic_auth(&creds.user, Some(&creds.pass))
        .send()
        .await
        .with_context(|| format!("GET {url} failed"))?;
    let code = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    Ok((code, body))
}

async fn obs_put_xml(
    client: &reqwest::Client,
    api_url: &str,
    path: &str,
    body: &str,
    creds: &ObsCredentials,
) -> Result<u16> {
    let url = format!("{api_url}{path}");
    let resp = client
        .put(&url)
        .basic_auth(&creds.user, Some(&creds.pass))
        .header("Content-Type", "application/xml")
        .body(body.to_string())
        .send()
        .await
        .with_context(|| format!("PUT {url} failed"))?;
    Ok(resp.status().as_u16())
}

async fn obs_post(
    client: &reqwest::Client,
    api_url: &str,
    path: &str,
    creds: &ObsCredentials,
) -> Result<u16> {
    let url = format!("{api_url}{path}");
    let resp = client
        .post(&url)
        .basic_auth(&creds.user, Some(&creds.pass))
        .send()
        .await
        .with_context(|| format!("POST {url} failed"))?;
    Ok(resp.status().as_u16())
}

async fn package_exists(
    client: &reqwest::Client,
    api_url: &str,
    project: &str,
    package: &str,
    creds: &ObsCredentials,
) -> bool {
    let path = format!("/source/{project}/{package}");
    matches!(obs_get(client, api_url, &path, creds).await, Ok((200, _)))
}

async fn create_meta(
    client: &reqwest::Client,
    api_url: &str,
    project: &str,
    package: &str,
    creds: &ObsCredentials,
) -> Result<()> {
    let meta = format!(
        r#"<package name="{package}" project="{project}">
  <title>{package}</title>
  <description>Auto-generated package for {package}</description>
</package>"#
    );
    let path = format!("/source/{project}/{package}/_meta");
    let code = obs_put_xml(client, api_url, &path, &meta, creds).await?;
    if code != 200 {
        bail!("create meta failed: HTTP {code}");
    }
    Ok(())
}

async fn upload_service(
    client: &reqwest::Client,
    api_url: &str,
    project: &str,
    package: &str,
    revision: &str,
    repo_url: &str,
    creds: &ObsCredentials,
) -> Result<()> {
    let extract = format!("SPECS/{package}/*");
    let service = format!(
        r#"<services>
  <service name="obs_scm">
    <param name="scm">git</param>
    <param name="url">{repo_url}</param>
    <param name="revision">{revision}</param>
    <param name="extract">{extract}</param>
  </service>
  <service name="download_files"/>
</services>"#
    );
    let path = format!("/source/{project}/{package}/_service");
    let code = obs_put_xml(client, api_url, &path, &service, creds).await?;
    if code != 200 {
        bail!("upload _service failed: HTTP {code}");
    }
    Ok(())
}

async fn commit_package(
    client: &reqwest::Client,
    api_url: &str,
    project: &str,
    package: &str,
    action: &str,
    creds: &ObsCredentials,
) -> Result<()> {
    let comment = format!("{action} {package} package");
    let path = format!("/source/{project}/{package}?cmd=commit&comment={comment}");
    let code = obs_post(client, api_url, &path, creds).await?;
    if code != 200 {
        bail!("commit failed: HTTP {code}");
    }
    Ok(())
}

async fn trigger_rebuild(
    client: &reqwest::Client,
    api_url: &str,
    project: &str,
    package: &str,
    creds: &ObsCredentials,
) -> Result<()> {
    let path = format!("/build/{project}?cmd=rebuild&package={package}");
    match obs_post(client, api_url, &path, creds).await {
        Ok(200) => Ok(()),
        Ok(code) => {
            warn!(
                http_code = code,
                "rebuild trigger returned non-200; build may still proceed"
            );
            Ok(())
        }
        Err(e) => {
            warn!(error = %e, "rebuild trigger failed; build may still proceed");
            Ok(())
        }
    }
}

#[allow(dead_code)]
pub async fn download_build_log(
    api_url: &str,
    project: &str,
    package: &str,
    repository: &str,
    arch: &str,
    creds: &ObsCredentials,
) -> Result<String> {
    let client = reqwest::Client::new();
    let path = format!("/build/{project}/{repository}/{arch}/{package}/_log");
    let (code, body) = obs_get(&client, api_url, &path, creds).await?;
    if code == 200 {
        Ok(body)
    } else {
        bail!("download build log failed: HTTP {code}")
    }
}

pub async fn ebf_submit(
    project: &str,
    revision: &str,
    components: &[String],
    api_url: &str,
    repo_url: &str,
    creds: &ObsCredentials,
) -> Result<EbfSubmitResult> {
    let client = reqwest::Client::new();
    let total = components.len();
    let mut success_count = 0;
    let mut failed = Vec::new();
    let mut messages = Vec::new();

    for component in components {
        let exists = package_exists(&client, api_url, project, component, creds).await;
        messages.push(format!(
            "[ebf] {component}: {} -> ",
            if exists { "existing" } else { "new" }
        ));

        if !exists {
            if let Err(e) = create_meta(&client, api_url, project, component, creds).await {
                messages.push(format!("create_meta failed: {e}\n"));
                failed.push(component.clone());
                continue;
            }
            messages.push("meta_created ".to_string());
        }

        match upload_service(
            &client, api_url, project, component, revision, repo_url, creds,
        )
        .await
        {
            Ok(()) => messages.push("service_uploaded ".to_string()),
            Err(e) => {
                messages.push(format!("service_upload failed: {e}\n"));
                failed.push(component.clone());
                continue;
            }
        }

        let action = if exists { "update" } else { "create" };
        match commit_package(&client, api_url, project, component, action, creds).await {
            Ok(()) => messages.push("committed ".to_string()),
            Err(e) => {
                messages.push(format!("commit failed: {e}\n"));
                failed.push(component.clone());
                continue;
            }
        }

        let _ = trigger_rebuild(&client, api_url, project, component, creds).await;
        messages.push("rebuild_triggered\n".to_string());
        success_count += 1;
    }

    let stdout = messages.join("");
    let success = failed.is_empty();
    let stderr = if !failed.is_empty() {
        format!("failed components: {}", failed.join(", "))
    } else {
        String::new()
    };

    Ok(EbfSubmitResult {
        success,
        returncode: if success { 0 } else { 1 },
        log_path: PathBuf::from("logs/ebf_submit.log"),
        stdout,
        stderr,
        success_count,
        total_count: total,
    })
}

#[derive(Debug, Clone)]
pub struct EbfSubmitResult {
    #[allow(dead_code)]
    pub success: bool,
    #[allow(dead_code)]
    pub returncode: i32,
    #[allow(dead_code)]
    pub log_path: PathBuf,
    pub stdout: String,
    pub stderr: String,
    pub success_count: usize,
    pub total_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_osc_credentials_from_sample() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            "[https://example.com]\nuser=testuser\npass=testpass\n",
        )
        .unwrap();
        let creds = read_osc_credentials(tmp.path()).unwrap();
        assert_eq!(creds.user, "testuser");
        assert_eq!(creds.pass, "testpass");
    }

    #[test]
    fn read_osc_credentials_missing_file() {
        let result = read_osc_credentials(Path::new("/nonexistent/oscrc"));
        assert!(result.is_err());
    }
}
