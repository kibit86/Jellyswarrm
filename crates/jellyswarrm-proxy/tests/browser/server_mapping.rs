use super::*;
use sqlx::Row;

const LOCAL_USERNAME: &str = "browser-mapping-user";
const LOCAL_PASSWORD: &str = "browser-mapping-password";
const TARGET_SERVER: &str = "Movies 1";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker, Git LFS media, embedded Jellyfin Web, and Chrome"]
async fn user_connects_to_backend_with_username_and_password_in_ui() -> Result<()> {
    run_mapping_scenario("password-mapping", false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker, Git LFS media, embedded Jellyfin Web, and Chrome"]
async fn user_connects_to_backend_with_quick_connect_in_ui() -> Result<()> {
    run_mapping_scenario("quick-connect-mapping", true).await
}

async fn run_mapping_scenario(name: &str, quick_connect: bool) -> Result<()> {
    let fixture = ServerFixture::start(false).await?;
    let upstream = upstream_urls(&fixture._compose)
        .await?
        .into_iter()
        .find(|(server_name, _, _)| *server_name == TARGET_SERVER)
        .context("Movies 1 backend missing")?
        .1;
    let backend_auth = if quick_connect {
        let auth = success_json(
            login_as(
                &fixture.client,
                &upstream,
                USERNAME,
                PASSWORD,
                DIRECT_AUTHORIZATION,
            )
            .await?,
        )
        .await?;
        ensure_backend_quick_connect_enabled(&fixture.client, &upstream).await?;
        Some(auth)
    } else {
        None
    };

    let playwright = Playwright::launch().await?;
    let mut launch_options =
        LaunchOptions::default().headless(std::env::var_os("JELLYSWARRM_BROWSER_HEADED").is_none());
    launch_options = match std::env::var("JELLYSWARRM_CHROME_PATH") {
        Ok(path) => launch_options.executable_path(path),
        Err(_) => launch_options.channel("chrome".to_owned()),
    };
    let container_mode = std::env::var_os("JELLYSWARRM_BROWSER_CONTAINERS").is_some();
    if !container_mode {
        playwright_rs::install_browsers(Some(&["ffmpeg"])).await?;
    }
    let container = if container_mode {
        Some(containers::ContainerBrowser::start().await?)
    } else {
        None
    };
    let browser = containers::launch(&playwright, launch_options, container.as_ref()).await?;
    let result = async {
        let video_dir = tempfile::tempdir()?;
        let video_path = if container.is_some() {
            "/tmp/videos".to_owned()
        } else {
            video_dir.path().to_string_lossy().into_owned()
        };
        let context = browser
            .new_context_with_options(
                playwright_rs::protocol::BrowserContextOptions::builder()
                    .record_video(
                        playwright_rs::protocol::RecordVideo::new(video_path).size(
                            playwright_rs::protocol::Viewport {
                                width: 1600,
                                height: 1000,
                            },
                        ),
                    )
                    .build(),
            )
            .await?;
        let page = prepare(&context, "mapping").await?;
        let artifacts = workspace_root().join("target/browser-test-artifacts").join(name);
        tokio::fs::create_dir_all(&artifacts).await?;
        let mut result = tokio::time::timeout(Duration::from_secs(180), async {
            create_local_user(&page, &fixture.proxy_url).await?;
            login_to_ui(&page, &fixture.proxy_url, LOCAL_USERNAME, LOCAL_PASSWORD).await?;
            page.wait_for_function(
                "() => typeof window.openConnectModal === 'function' && !!document.querySelector('#main-content #connect_modal') && !!document.querySelector('#main-content tr .connect-button')",
                None,
            )
            .await?;
            let row = page.locator("#main-content tr:has(th:has-text('Movies 1'))");
            row.locator(".connect-button").click(None).await?;
            if quick_connect {
                page.locator("#quick_connect_method")
                    .click(None)
                    .await?;
                page.wait_for_function(
                    "() => document.querySelector('#quick_connect_code')?.textContent?.trim().length === 6",
                    None,
                )
                .await?;
                let code = page.locator("#quick_connect_code").inner_text().await?;
                let auth = backend_auth.as_ref().context("missing backend login")?;
                let token = required_string(auth, "/AccessToken")?;
                let response = fixture
                    .client
                    .post(format!("{upstream}/QuickConnect/Authorize"))
                    .header("Authorization", format!("{DIRECT_AUTHORIZATION}, Token=\"{token}\""))
                    .query(&[("code", code.trim())])
                    .send()
                    .await?;
                let approved = success_json(response).await?;
                anyhow::ensure!(approved == true, "backend did not approve code: {approved}");
            } else {
                page.locator("#connect_form input[name=username]")
                    .fill(USERNAME, None)
                    .await?;
                page.locator("#connect_form input[name=password]")
                    .fill(PASSWORD, None)
                    .await?;
                page.locator("#connect_form button[type=submit]")
                    .click(None)
                    .await?;
            }
            page.wait_for_function(
                r#"() => !![...document.querySelectorAll('#main-content tr')].find(row => row.querySelector('th')?.textContent?.includes('Movies 1') && row.querySelector('button[hx-delete*="/user/servers/"]'))"#,
                None,
            )
            .await?;
            // The status check must succeed, not merely show a mapping row.
            page.wait_for_function(
                "() => [...document.querySelectorAll('#main-content tr')].some(row => row.querySelector('th')?.textContent?.includes('Movies 1') && row.querySelector('td .fa-check-circle') && row.textContent?.includes('test'))",
                None,
            )
            .await?;

            // A simple user is never offered a server's panels: their mapped account is an
            // ordinary one on the backend, and "Open"/"Set up" are the administrator's.
            page.wait_for_function(
                "() => !!document.querySelector('#main-content table') && !document.querySelector('#main-content a[href*=\"/backend/\"]')",
                None,
            )
            .await?;

            let db_path = fixture._proxy._data_dir.path().join("jellyswarrm.db");
            let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", db_path.display())).await?;
            let mapping = sqlx::query(
                "SELECT sm.id, sm.auth_method, sm.mapped_username, sm.mapped_password, sm.backend_user_id, sm.encrypted_token \
                 FROM server_mappings sm JOIN users u ON u.id = sm.user_id \
                 JOIN servers s ON s.id = sm.server_id WHERE u.original_username = ? AND s.name = ?",
            )
            .bind(LOCAL_USERNAME)
            .bind(TARGET_SERVER)
            .fetch_one(&pool)
            .await?;
            assert_eq!(mapping.get::<String, _>("mapped_username"), USERNAME);
            if quick_connect {
                assert_eq!(mapping.get::<String, _>("auth_method"), "quick_connect");
                assert_eq!(
                    mapping.get::<Option<String>, _>("backend_user_id").as_deref(),
                    Some(required_string(backend_auth.as_ref().unwrap(), "/User/Id")?),
                );
                let encrypted = mapping.get::<Option<String>, _>("encrypted_token").context("token was not stored")?;
                anyhow::ensure!(!encrypted.is_empty() && encrypted != required_string(backend_auth.as_ref().unwrap(), "/AccessToken")?, "token was not encrypted");
                assert_eq!(mapping.get::<String, _>("mapped_password"), "");
            } else {
                assert_eq!(mapping.get::<String, _>("auth_method"), "password");
                assert!(mapping.get::<Option<String>, _>("encrypted_token").is_none());
                assert_ne!(mapping.get::<String, _>("mapped_password"), PASSWORD);
            }

            // A fresh client login exercises the stored mapping and creates a
            // real backend authorization session, not just a saved UI row.
            let login = success_json(login_as(
                &fixture.client, &fixture.proxy_url, LOCAL_USERNAME, LOCAL_PASSWORD,
                AUTHORIZATION,
            ).await?).await?;
            assert_eq!(required_string(&login, "/User/Name")?, LOCAL_USERNAME);
            let backend_session: (String, String) = sqlx::query_as(
                "SELECT auth.original_user_id, auth.jellyfin_token \
                 FROM authorization_sessions auth WHERE auth.mapping_id = ?",
            )
            .bind(mapping.get::<i64, _>("id"))
            .fetch_one(&pool)
            .await?;
            if quick_connect {
                assert_eq!(backend_session.0, required_string(backend_auth.as_ref().unwrap(), "/User/Id")?);
            }
            anyhow::ensure!(!backend_session.1.is_empty(), "no backend token in client session");
            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("mapping browser scenario exceeded three minutes")
        .and_then(|result| result);

        if result.is_err() {
            if let Err(error) = page.screenshot_to_file(&artifacts.join("mapping.png"), None).await {
                eprintln!("Could not save mapping screenshot: {error}");
            }
        }
        if let Err(error) = containers::save_trace(&context, container.as_ref(), artifacts.join("mapping.zip")).await {
            eprintln!("Could not save mapping trace: {error}");
            if result.is_ok() { result = Err(error.context("export mapping trace")); }
        }
        if let Err(error) = containers::save_video(&context, &page, container.as_ref(), artifacts.join("mapping.webm")).await {
            eprintln!("Could not save mapping video: {error}");
            if result.is_ok() { result = Err(error.context("export mapping video")); }
        }
        result
    }.await;
    let closed = browser.close().await;
    result?;
    closed?;
    Ok(())
}

async fn create_local_user(page: &Page, base: &str) -> Result<()> {
    login_to_ui(page, base, PROXY_ADMIN_USERNAME, PROXY_ADMIN_PASSWORD).await?;
    page.locator("a[hx-get='/ui/users']").click(None).await?;
    page.locator("#add-user input[name=username]")
        .fill(LOCAL_USERNAME, None)
        .await?;
    page.locator("#add-user input[name=password]")
        .fill(LOCAL_PASSWORD, None)
        .await?;
    page.locator("#add-user button[type=submit]")
        .click(None)
        .await?;
    page.wait_for_function(
        r#"() => !!document.querySelector('#user-list [data-username="browser-mapping-user"]')"#,
        None,
    )
    .await?;
    page.goto(&format!("{base}/ui/logout"), None).await?;
    Ok(())
}

async fn login_to_ui(page: &Page, base: &str, username: &str, password: &str) -> Result<()> {
    page.goto(&format!("{base}/ui/login"), None).await?;
    page.locator("#username").fill(username, None).await?;
    page.locator("form input[name=password]")
        .fill(password, None)
        .await?;
    page.locator("form button[type=submit]").click(None).await?;
    page.wait_for_function(
        "() => location.pathname !== '/ui/login' && !!document.querySelector('#main-content')",
        None,
    )
    .await?;
    Ok(())
}

async fn ensure_backend_quick_connect_enabled(client: &Client, upstream: &str) -> Result<()> {
    let response = client
        .get(format!("{upstream}/QuickConnect/Enabled"))
        .send()
        .await?;
    if success_json(response).await? == true {
        return Ok(());
    }
    let admin =
        success_json(login_as(client, upstream, "admin", "password", DIRECT_AUTHORIZATION).await?)
            .await?;
    let token = required_string(&admin, "/AccessToken")?;
    let authorization = format!("{DIRECT_AUTHORIZATION}, Token=\"{token}\"");
    let mut config = success_json(
        client
            .get(format!("{upstream}/System/Configuration"))
            .header("Authorization", &authorization)
            .send()
            .await?,
    )
    .await?;
    config["QuickConnectAvailable"] = true.into();
    let response = client
        .post(format!("{upstream}/System/Configuration"))
        .header("Authorization", authorization)
        .json(&config)
        .send()
        .await?;
    anyhow::ensure!(
        response.status().is_success(),
        "could not enable backend Quick Connect: {}",
        response.status()
    );
    Ok(())
}

fn row_js(server: &str) -> String {
    format!(
        "[...document.querySelectorAll('#main-content tr')].find(r => r.querySelector('th')?.textContent?.includes('{server}'))"
    )
}

/// The proxy administrator lands on "My servers", can open a server that has no mapped
/// account yet, connects the backend's administrator account, and then "Administer"
/// opens that server's own dashboard already signed in (new tab, no second login).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker, Git LFS media, embedded Jellyfin Web, and Chrome"]
async fn administrator_sees_their_servers_and_opens_the_signed_in_panels() -> Result<()> {
    let fixture = ServerFixture::start(false).await?;
    let playwright = Playwright::launch().await?;
    let mut launch_options =
        LaunchOptions::default().headless(std::env::var_os("JELLYSWARRM_BROWSER_HEADED").is_none());
    launch_options = match std::env::var("JELLYSWARRM_CHROME_PATH") {
        Ok(path) => launch_options.executable_path(path),
        Err(_) => launch_options.channel("chrome".to_owned()),
    };
    let container_mode = std::env::var_os("JELLYSWARRM_BROWSER_CONTAINERS").is_some();
    if !container_mode {
        playwright_rs::install_browsers(Some(&["ffmpeg"])).await?;
    }
    let container = if container_mode {
        Some(containers::ContainerBrowser::start().await?)
    } else {
        None
    };
    let browser = containers::launch(&playwright, launch_options, container.as_ref()).await?;
    let result = async {
        let context = browser.new_context().await?;
        let page = prepare(&context, "panels").await?;
        let artifacts = workspace_root().join("target/browser-test-artifacts/panels");
        tokio::fs::create_dir_all(&artifacts).await?;
        let mut result = tokio::time::timeout(
            Duration::from_secs(240),
            panels_scenario(&page, &fixture.proxy_url),
        )
        .await
        .context("panels browser scenario exceeded four minutes")
        .and_then(|result| result);
        if result.is_err() {
            if let Err(error) = page
                .screenshot_to_file(&artifacts.join("panels.png"), None)
                .await
            {
                eprintln!("Could not save panels screenshot: {error}");
            }
        }
        if let Err(error) =
            containers::save_trace(&context, container.as_ref(), artifacts.join("panels.zip")).await
        {
            eprintln!("Could not save panels trace: {error}");
            if result.is_ok() {
                result = Err(error.context("export panels trace"));
            }
        }
        result
    }
    .await;
    let closed = browser.close().await;
    result?;
    closed?;
    Ok(())
}

async fn panels_scenario(page: &Page, base: &str) -> Result<()> {
    let movies = row_js(TARGET_SERVER);

    // The administrator lands on their own servers...
    login_to_ui(page, base, PROXY_ADMIN_USERNAME, PROXY_ADMIN_PASSWORD).await?;
    page.wait_for_function(
        "() => document.querySelector('#main-content')?.textContent?.includes('Your Connected Servers')",
        None,
    )
    .await?;
    // ...and, with no mapped account yet, can open any server on its own page (new tab).
    // The fixture servers are all set up, so it is "Open", never "Set up", and there is
    // no dashboard to administer yet.
    page.wait_for_function(
        &format!(
            r##"() => {{ const row = {movies}; const open = row?.querySelector('a[role=button][href$="/web/"]'); return !!open && open.target === '_blank' && !/Set up/.test(row.textContent) && !row.querySelector('a[href*="#/dashboard"]'); }}"##
        ),
        None,
    )
    .await?;
    let open_href = page
        .evaluate_value(&format!(
            r#"{movies}.querySelector('a[role=button][href$="/web/"]').getAttribute('href')"#
        ))
        .await?;
    let open_href = open_href.trim_matches('"').to_owned();
    anyhow::ensure!(
        open_href.starts_with("/ui/backend/") && open_href.ends_with("/web/"),
        "unexpected Open link: {open_href}"
    );
    page.goto(&format!("{base}{open_href}"), None).await?;
    // The backend's own client is served under the proxy, with its storage kept apart.
    page.wait_for_function(
        "() => document.documentElement.outerHTML.includes('jsw-backend-')",
        None,
    )
    .await?;

    // Connect the backend's administrator account to the proxy administrator.
    page.goto(&format!("{base}/ui"), None).await?;
    page.wait_for_function(
        "() => typeof window.openConnectModal === 'function' && !!document.querySelector('#main-content #connect_modal')",
        None,
    )
    .await?;
    page.locator("#main-content tr:has(th:has-text('Movies 1'))")
        .locator(".connect-button")
        .click(None)
        .await?;
    page.locator("#connect_form input[name=username]")
        .fill("admin", None)
        .await?;
    page.locator("#connect_form input[name=password]")
        .fill("password", None)
        .await?;
    page.locator("#connect_form button[type=submit]")
        .click(None)
        .await?;

    // The server moves to "Your Connected Servers" and, as the mapped account administers
    // it, now has "Administer" (new tab, straight to the dashboard).
    page.wait_for_function(
        &format!(
            r##"() => {{ const row = {movies}; const link = row?.querySelector('a[href$="#/dashboard"]'); return !!row?.querySelector('button[hx-delete*="/user/servers/"]') && !!link && link.target === '_blank'; }}"##
        ),
        None,
    )
    .await?;
    let administer = page
        .evaluate_value(&format!(
            r##"{movies}.querySelector('a[href$="#/dashboard"]').getAttribute('href')"##
        ))
        .await?;
    let administer = administer.trim_matches('"').to_owned();
    anyhow::ensure!(
        administer.starts_with("/ui/backend/") && administer.ends_with("/web/#/dashboard"),
        "unexpected Administer link: {administer}"
    );

    // Opening it hands the page the mapped account's session: no second login.
    page.goto(&format!("{base}{administer}"), None).await?;
    page.wait_for_function(
        r#"() => { try { const c = JSON.parse(localStorage.getItem('jellyfin_credentials')); return !!(c && c.Servers && c.Servers[0] && c.Servers[0].AccessToken); } catch (_) { return false; } }"#,
        None,
    )
    .await?;
    // The client stays on the dashboard instead of falling back to a sign-in page.
    tokio::time::sleep(Duration::from_secs(4)).await;
    let hash = page.evaluate_value("location.hash").await?;
    anyhow::ensure!(
        hash.contains("dashboard"),
        "the dashboard did not stay open, location.hash = {hash}"
    );
    Ok(())
}
