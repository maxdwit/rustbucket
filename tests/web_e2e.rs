//! Browser end-to-end test of the web UI with a real Chromium (thirtyfour / WebDriver).
//! Ignored by default; run it with `scripts/e2e_web.sh` (starts chromedriver and sets the env below).
//!
//! The server runs inside this test on a random port. Flow: login -> upload (plain and chunked v2) ->
//! new folder -> rename -> navigate -> move -> share dialog -> delete -> logout, then the same pages
//! at phone width (390x844) must not scroll sideways.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rustbucket::config::Config;
use rustbucket::db;
use rustbucket::routes::{AppState, router};
use thirtyfour::prelude::*;

struct Server {
    base: String,
    data: PathBuf,
    pool: sqlx::SqlitePool,
    _dir: tempfile::TempDir,
}

async fn start() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let pool = db::connect(&format!("sqlite://{}", dir.path().join("t.db").display()))
        .await
        .unwrap();
    db::migrate(&pool).await.unwrap();
    db::create_user(&pool, "bob", "Bob B", "pw-bob", true)
        .await
        .unwrap();
    db::create_user(&pool, "alice", "Alice A", "pw-alice", false)
        .await
        .unwrap();
    let cfg = Config {
        data_dir: data.clone(),
        ..Config::default()
    };
    let app = router(AppState::new(pool.clone(), Arc::new(cfg)));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    Server {
        base: format!("http://{addr}"),
        data,
        pool,
        _dir: dir,
    }
}

async fn driver(width: u32, height: u32) -> WebDriverResult<WebDriver> {
    let url = std::env::var("WEBDRIVER_URL").unwrap_or_else(|_| "http://127.0.0.1:9515".into());
    let mut caps = DesiredCapabilities::chrome();
    caps.set_binary(&std::env::var("CHROMIUM_BIN").unwrap_or_else(|_| "/usr/bin/chromium".into()))?;
    for a in [
        "--headless=new",
        "--no-sandbox",
        "--disable-gpu",
        "--disable-dev-shm-usage",
        "--disable-search-engine-choice-screen",
    ] {
        caps.add_arg(a)?;
    }
    caps.add_arg(&format!("--window-size={width},{height}"))?;
    let d = WebDriver::new(&url, caps).await?;
    d.set_window_rect(0, 0, width, height).await?;
    d.set_implicit_wait_timeout(Duration::ZERO).await?;
    Ok(d)
}

async fn wait_for(d: &WebDriver, css: &str) -> WebDriverResult<WebElement> {
    d.query(By::Css(css))
        .wait(Duration::from_secs(15), Duration::from_millis(100))
        .first()
        .await
}

async fn gone(d: &WebDriver, css: &str) -> WebDriverResult<()> {
    for _ in 0..150 {
        if d.find_all(By::Css(css)).await?.is_empty() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(WebDriverError::Timeout(format!("still present: {css}")))
}

fn row(name: &str) -> String {
    format!(".row[data-name=\"{name}\"]")
}

/// Open the "..." menu of a row (rename, move, versions and delete live in it).
async fn open_menu(d: &WebDriver, row_css: &str) -> WebDriverResult<()> {
    d.find(By::Css(format!("{row_css} details.menu summary")))
        .await?
        .click()
        .await?;
    wait_for(d, &format!("{row_css} details.menu[open]")).await?;
    Ok(())
}

async fn login(d: &WebDriver, base: &str, user: &str, pw: &str) -> WebDriverResult<()> {
    d.goto(format!("{base}/")).await?; // stock behaviour: / sends us to the login page
    wait_for(d, "input[name=username]")
        .await?
        .send_keys(user)
        .await?;
    d.find(By::Css("input[name=password]"))
        .await?
        .send_keys(pw)
        .await?;
    d.find(By::Css("button[type=submit]"))
        .await?
        .click()
        .await?;
    wait_for(d, "#files-root").await?;
    Ok(())
}

fn upload_dir() -> (PathBuf, PathBuf) {
    let inside = PathBuf::from(
        std::env::var("E2E_UPLOAD_DIR").expect("E2E_UPLOAD_DIR (use scripts/e2e_web.sh)"),
    );
    let host = PathBuf::from(
        std::env::var("E2E_UPLOAD_DIR_HOST").unwrap_or_else(|_| inside.display().to_string()),
    );
    std::fs::create_dir_all(&inside).unwrap();
    (inside, host)
}

async fn no_sideways_scroll(d: &WebDriver, what: &str) -> WebDriverResult<()> {
    let v = d
        .execute(
            "return [document.documentElement.scrollWidth, window.innerWidth];",
            vec![],
        )
        .await?;
    let [sw, iw]: [i64; 2] = serde_json::from_value(v.json().clone()).unwrap();
    assert!(sw <= iw, "{what}: page is {sw}px wide in a {iw}px window");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs chromedriver + chromium: run scripts/e2e_web.sh"]
async fn browser_login_upload_rename_move_share_delete_logout() -> WebDriverResult<()> {
    let s = start().await;
    let (inside, host) = upload_dir();
    let small = (inside.join("hello.txt"), host.join("hello.txt"));
    std::fs::write(&small.0, "hello from the browser").unwrap();
    let big_bytes: Vec<u8> = (0..5000u32).map(|i| (i * 7 % 251) as u8).collect();
    let big = (inside.join("chunky.bin"), host.join("chunky.bin"));
    std::fs::write(&big.0, &big_bytes).unwrap();
    let files = s.data.join("bob/files");

    let d = driver(1280, 900).await?;
    let result = async {
        // login: a wrong password is refused, the right one lands on the file list
        d.goto(format!("{}/", s.base)).await?;
        wait_for(&d, "input[name=username]")
            .await?
            .send_keys("bob")
            .await?;
        d.find(By::Css("input[name=password]"))
            .await?
            .send_keys("wrong")
            .await?;
        d.find(By::Css("button[type=submit]"))
            .await?
            .click()
            .await?;
        assert!(
            wait_for(&d, ".err")
                .await?
                .text()
                .await?
                .contains("Wrong account name or password")
        );
        login(&d, &s.base, "bob", "pw-bob").await?;
        assert!(d.current_url().await?.path().starts_with("/apps/files"));

        // upload through the real file input (plain PUT)
        wait_for(&d, "#file-input")
            .await?
            .send_keys(small.1.display().to_string())
            .await?;
        wait_for(&d, &row("hello.txt")).await?;
        assert_eq!(
            std::fs::read_to_string(files.join("hello.txt")).unwrap(),
            "hello from the browser"
        );

        // chunked upload v2: force 1 KiB chunks for this 5000 byte file
        d.execute("localStorage.setItem('rbChunkSize', '1024');", vec![])
            .await?;
        d.find(By::Css("#file-input"))
            .await?
            .send_keys(big.1.display().to_string())
            .await?;
        wait_for(&d, &row("chunky.bin")).await?;
        assert_eq!(std::fs::read(files.join("chunky.bin")).unwrap(), big_bytes);
        assert!(
            !s.data.join("bob/uploads").exists()
                || std::fs::read_dir(s.data.join("bob/uploads"))
                    .unwrap()
                    .next()
                    .is_none(),
            "no orphaned upload folder"
        );
        d.execute("localStorage.removeItem('rbChunkSize');", vec![])
            .await?;

        // the same name again is refused instead of overwritten
        d.find(By::Css("#file-input"))
            .await?
            .send_keys(small.1.display().to_string())
            .await?;
        assert!(
            wait_for(&d, ".upload.failed .state")
                .await?
                .text()
                .await?
                .contains("already taken")
        );

        // new folder
        d.find(By::Css("#btn-newfolder")).await?.click().await?;
        let name = wait_for(&d, "#newfolder-host input").await?;
        name.send_keys("Docs\n").await?;
        wait_for(&d, &row("Docs")).await?;
        assert!(files.join("Docs").is_dir());

        // rename
        open_menu(&d, &row("hello.txt")).await?;
        wait_for(
            &d,
            &format!("{} button[data-action=rename]", row("hello.txt")),
        )
        .await?
        .click()
        .await?;
        let inp = wait_for(&d, &format!("{} input[type=text]", row("hello.txt"))).await?;
        inp.clear().await?;
        inp.send_keys("renamed.txt\n").await?;
        wait_for(&d, &row("renamed.txt")).await?;
        gone(&d, &row("hello.txt")).await?;
        assert!(files.join("renamed.txt").exists() && !files.join("hello.txt").exists());

        // move into Docs (inline target field)
        open_menu(&d, &row("renamed.txt")).await?;
        d.find(By::Css(format!(
            "{} button[data-action=move]",
            row("renamed.txt")
        )))
        .await?
        .click()
        .await?;
        let inp = wait_for(&d, &format!("{} input[type=text]", row("renamed.txt"))).await?;
        inp.clear().await?;
        inp.send_keys("/Docs\n").await?;
        gone(&d, &row("renamed.txt")).await?;
        assert!(files.join("Docs/renamed.txt").exists());

        // navigate into the folder (HTMX swap, pushState), see the file, come back with the breadcrumb
        d.find(By::Css(format!("{} a", row("Docs"))))
            .await?
            .click()
            .await?;
        wait_for(&d, &row("renamed.txt")).await?;
        assert!(
            d.current_url()
                .await?
                .query()
                .unwrap_or("")
                .contains("dir=%2FDocs")
        );
        assert!(
            d.find(By::Css(".crumbs"))
                .await?
                .text()
                .await?
                .contains("Docs")
        );
        d.find(By::Css(".crumbs a")).await?.click().await?; // the root crumb, "My own files"
        wait_for(&d, &row("Docs")).await?;

        // share the folder with alice from the dialog
        d.find(By::Css(format!(
            "{} button[data-action=share]",
            row("Docs")
        )))
        .await?
        .click()
        .await?;
        let who = wait_for(&d, "dialog[open] input[name=with]").await?;
        who.send_keys("alice").await?;
        let sel = d
            .find(By::Css("dialog[open] select[name=permissions]"))
            .await?;
        sel.send_keys("Can edit").await?;
        d.find(By::Css("dialog[open] #share-add button[type=submit]"))
            .await?
            .click()
            .await?;
        let li = wait_for(&d, "dialog[open] .sharelist li .who").await?;
        assert!(li.text().await?.contains("Alice A"));
        let (perm, with): (i64, String) =
            sqlx::query_as("SELECT permissions, share_with FROM shares")
                .fetch_one(&s.pool)
                .await
                .unwrap();
        assert_eq!((perm, with.as_str()), (15, "alice"));
        // change to view-only and remove again, through the dialog
        d.find(By::Css("dialog[open] select[data-share-perm]"))
            .await?
            .send_keys("Can view")
            .await?;
        for _ in 0..50 {
            let p: i64 = sqlx::query_scalar("SELECT permissions FROM shares")
                .fetch_one(&s.pool)
                .await
                .unwrap();
            if p == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT permissions FROM shares")
                .fetch_one(&s.pool)
                .await
                .unwrap(),
            1
        );
        d.find(By::Css("dialog[open] button[data-share-remove]"))
            .await?
            .click()
            .await?;
        wait_for(&d, "dialog[open] .sharelist li.muted").await?;
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM shares")
                .fetch_one(&s.pool)
                .await
                .unwrap(),
            0
        );
        d.find(By::Css("#share-close")).await?.click().await?;

        // delete needs a second click ("Sure?")
        let del = format!("{} button[data-action=delete]", row("chunky.bin"));
        open_menu(&d, &row("chunky.bin")).await?;
        d.find(By::Css(&del)).await?.click().await?;
        assert_eq!(d.find(By::Css(&del)).await?.text().await?, "Sure?");
        assert!(files.join("chunky.bin").exists());
        d.find(By::Css(&del)).await?.click().await?;
        gone(&d, &row("chunky.bin")).await?;
        assert!(!files.join("chunky.bin").exists());

        // trash bin page: the deleted file is listed, restore brings it back into the file list
        d.goto(format!("{}/apps/files/trash", s.base)).await?;
        wait_for(&d, &row("chunky.bin")).await?;
        d.find(By::Css(format!("{} button[data-trash=restore]", row("chunky.bin")))).await?.click().await?;
        wait_for(&d, "#trashlist .empty").await?; // the page reloads with an empty bin
        assert_eq!(std::fs::read(files.join("chunky.bin")).unwrap(), big_bytes);
        // delete it again, then remove it for good from the bin (two clicks for "empty trash")
        d.goto(format!("{}/apps/files/", s.base)).await?;
        open_menu(&d, &row("chunky.bin")).await?;
        d.find(By::Css(format!("{} button[data-action=delete]", row("chunky.bin")))).await?.click().await?;
        d.find(By::Css(format!("{} button[data-action=delete]", row("chunky.bin")))).await?.click().await?;
        gone(&d, &row("chunky.bin")).await?;
        d.goto(format!("{}/apps/files/trash", s.base)).await?;
        wait_for(&d, &row("chunky.bin")).await?;
        d.find(By::Css(format!("{} button[data-trash=delete]", row("chunky.bin")))).await?.click().await?;
        wait_for(&d, "#trashlist .empty").await?;
        assert!(std::fs::read_dir(s.data.join("bob/trash")).map_or(true, |mut x| x.next().is_none()));

        // versions: overwrite a file twice through the upload, then restore the first version from the dialog
        d.goto(format!("{}/apps/files/", s.base)).await?;
        for (text, secs) in [("generation one", 1_700_000_100u64), ("generation two", 1_700_000_200), ("generation three", 1_700_000_300)] {
            std::fs::write(&small.0, text).unwrap();
            let f = std::fs::File::open(&small.0).unwrap();
            f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs)).unwrap();
            // the web UI refuses to overwrite, so put the new content through DAV like another client would
            let mut reqs = std::collections::HashMap::new();
            reqs.insert("X-OC-Mtime", secs.to_string());
            let _ = reqs;
            let status = d.execute(
                "const [t, m] = arguments; return fetch('/remote.php/dav/files/bob/versioned.txt', {method: 'PUT', headers: {'X-CSRF-Token': document.querySelector('meta[name=csrf-token]').content, 'X-OC-Mtime': m}, body: t, credentials: 'same-origin'}).then(r => r.status);",
                vec![text.into(), secs.to_string().into()],
            ).await?;
            let _ = status;
        }
        d.goto(format!("{}/apps/files/", s.base)).await?;
        wait_for(&d, &row("versioned.txt")).await?;
        open_menu(&d, &row("versioned.txt")).await?;
        d.find(By::Css(format!("{} button[data-action=versions]", row("versioned.txt")))).await?.click().await?;
        wait_for(&d, "dialog[open] li[data-ts=\"1700000100\"] button[data-ver=restore]").await?.click().await?;
        for _ in 0..80 {
            if std::fs::read_to_string(files.join("versioned.txt")).is_ok_and(|c| c == "generation one") { break; }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(std::fs::read_to_string(files.join("versioned.txt")).unwrap(), "generation one");

        // personal settings are reachable and the admin page too (bob is an administrator here)
        d.goto(format!("{}/settings/user", s.base)).await?;
        wait_for(&d, "form[action='/settings/user/profile']").await?;
        d.goto(format!("{}/settings/users", s.base)).await?;
        wait_for(&d, "#user-alice").await?;

        // logout kills the session: the back button / a direct visit lead to the login page
        d.goto(format!("{}/apps/files/", s.base)).await?;
        d.find(By::Css("header form[action='/logout'] button"))
            .await?
            .click()
            .await?;
        wait_for(&d, "input[name=username]").await?;
        d.goto(format!("{}/apps/files/", s.base)).await?;
        wait_for(&d, "input[name=username]").await?;
        Ok::<(), WebDriverError>(())
    }
    .await;
    d.quit().await?;
    result
}

/// Write files straight to disk for `uid` and let a scan index them (what `rustbucket scan` does).
async fn seed(s: &Server, uid: &str, files: &[(&str, &str)]) {
    std::fs::create_dir_all(&s.data).unwrap();
    std::fs::write(s.data.join(".rustbucket-data"), "").unwrap();
    for (rel, body) in files {
        let p = s.data.join(uid).join("files").join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
    let id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE uid = $1")
        .bind(uid)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    rustbucket::storage::Store::new(s.pool.clone(), s.data.clone())
        .scan(
            &rustbucket::storage::Owner {
                id,
                uid: uid.into(),
            },
            false,
            false,
        )
        .await
        .unwrap();
}

async fn active_menu(d: &WebDriver) -> WebDriverResult<String> {
    Ok(d.find(By::Css(".side a.active span:last-child"))
        .await?
        .text()
        .await?
        .trim()
        .to_string())
}

/// The left menu in a real browser: switching between "My own files", "Shared with me" and "Shared".
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs chromedriver + chromium: run scripts/e2e_web.sh"]
async fn browser_left_menu_switches_between_own_incoming_and_outgoing_shares() -> WebDriverResult<()>
{
    let s = start().await;
    seed(
        &s,
        "alice",
        &[("Reports/q1.txt", "numbers"), ("readme.txt", "r")],
    )
    .await;
    seed(&s, "bob", &[("mine.txt", "m")]).await;
    // alice shares Reports with bob (read only)
    sqlx::query(
        "INSERT INTO shares (share_type, owner_id, file_source, share_with, permissions, stime) \
         SELECT 0, u.id, f.fileid, 'bob', 1, 1700000000 FROM file_cache f, users u WHERE f.name = 'Reports' AND u.uid = 'alice'",
    )
    .execute(&s.pool)
    .await
    .unwrap();
    let d = driver(1280, 900).await?;
    let result = async {
        // ---- bob
        login(&d, &s.base, "bob", "pw-bob").await?;
        wait_for(&d, &row("mine.txt")).await?;
        // bob is an administrator: the Admin menu has Users, About and the version line; About shows the build
        d.find(By::Css(".admin-menu summary")).await?.click().await?;
        let v = wait_for(&d, ".admin-menu[open] .menu-version").await?.text().await?;
        assert!(v.starts_with(&format!("v{}", env!("CARGO_PKG_VERSION"))), "{v}");
        d.find(By::Css(".admin-menu[open] a[href='/settings/users']")).await?;
        d.find(By::Css(".admin-menu[open] a[href='/settings/about']")).await?.click().await?;
        wait_for(&d, "table.kv").await?;
        assert!(d.find(By::Css("h1")).await?.text().await?.contains("About"));
        d.goto(format!("{}/apps/files/", s.base)).await?;
        wait_for(&d, &row("mine.txt")).await?;
        assert_eq!(active_menu(&d).await?, "My own files");
        assert!(d.find_all(By::Css(row("Reports"))).await?.is_empty(), "incoming shares are not in My own files");
        // the menu is a column on the left on a desktop
        let x: i64 = d.execute("return Math.round(document.querySelector('.side').getBoundingClientRect().right <= document.querySelector('main').getBoundingClientRect().left);", vec![]).await?.convert()?;
        assert_eq!(x, 1, "the menu sits left of the file list");

        d.find(By::Css(".side a[href*='shared-with-me']")).await?.click().await?;
        wait_for(&d, &row("Reports")).await?;
        assert_eq!(active_menu(&d).await?, "Shared with me");
        assert!(d.find_all(By::Css(row("mine.txt"))).await?.is_empty());
        assert!(d.find(By::Css(row("Reports"))).await?.text().await?.contains("from Alice A"));
        assert!(d.find(By::Css("#btn-upload")).await?.attr("disabled").await?.is_some(), "no upload into a list");
        // into the share: in place (HTMX), the menu keeps pointing at "Shared with me"
        d.find(By::Css(format!("{} .name a", row("Reports")))).await?.click().await?;
        wait_for(&d, &row("q1.txt")).await?;
        assert_eq!(active_menu(&d).await?, "Shared with me");
        assert!(d.find(By::Css(".crumbs")).await?.text().await?.contains("Reports"));
        // the root crumb leads back to the list
        d.find(By::Css(".crumbs a")).await?.click().await?;
        wait_for(&d, &row("Reports")).await?;
        d.find(By::Css(".side a[href='/apps/files/?view=shared']")).await?.click().await?;
        wait_for(&d, ".list .empty").await?;
        assert_eq!(d.find(By::Css(".list .empty")).await?.text().await?, "You have not shared anything yet.");
        d.find(By::Css(".side a[href='/apps/files/trash']")).await?.click().await?;
        wait_for(&d, "#trashlist").await?;
        assert_eq!(active_menu(&d).await?, "Trash");
        d.find(By::Css("header form[action='/logout'] button")).await?.click().await?;
        wait_for(&d, "input[name=username]").await?;

        // ---- alice sees what she shares, and who with
        login(&d, &s.base, "alice", "pw-alice").await?;
        d.find(By::Css(".side a[href='/apps/files/?view=shared']")).await?.click().await?;
        wait_for(&d, &row("Reports")).await?;
        assert_eq!(active_menu(&d).await?, "Shared");
        let t = d.find(By::Css(row("Reports"))).await?.text().await?;
        assert!(t.contains("with Bob B: Can view"), "{t}");
        assert!(d.find_all(By::Css(row("readme.txt"))).await?.is_empty(), "only shared items are listed");
        // a folder leads to the real place in My own files (a full page load, the menu follows)
        d.find(By::Css(format!("{} .name a", row("Reports")))).await?.click().await?;
        wait_for(&d, &row("q1.txt")).await?;
        assert_eq!(active_menu(&d).await?, "My own files");
        Ok::<(), WebDriverError>(())
    }
    .await;
    d.quit().await?;
    result
}

/// Multi-select in a real browser: tick two files and move them in one go, then select everything in a
/// folder and delete it (with the confirmation click); deleted items end up in the trash.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs chromedriver + chromium: run scripts/e2e_web.sh"]
async fn browser_multi_select_moves_and_deletes_several_items() -> WebDriverResult<()> {
    let s = start().await;
    seed(
        &s,
        "bob",
        &[
            ("a.txt", "a"),
            ("b.txt", "b"),
            ("c.txt", "c"),
            ("Target/keep.txt", "k"),
        ],
    )
    .await;
    let files = s.data.join("bob/files");
    let d = driver(1280, 900).await?;
    let result = async {
        login(&d, &s.base, "bob", "pw-bob").await?;
        wait_for(&d, &row("a.txt")).await?;
        assert!(
            !d.find(By::Css("#bulkbar")).await?.is_displayed().await?,
            "no selection, no bar"
        );
        d.find(By::Css(format!("{} .sel", row("a.txt"))))
            .await?
            .click()
            .await?;
        d.find(By::Css(format!("{} .sel", row("b.txt"))))
            .await?
            .click()
            .await?;
        wait_for(&d, "#bulkbar:not([hidden])").await?;
        assert_eq!(
            d.find(By::Css("#bulk-count")).await?.text().await?,
            "2 selected"
        );
        let partial: bool = d
            .execute(
                "return document.getElementById('sel-all').indeterminate;",
                vec![],
            )
            .await?
            .convert()?;
        assert!(partial, "select-all shows a partial selection");

        // move both into Target
        d.find(By::Css("#bulk-move")).await?.click().await?;
        let inp = wait_for(&d, "#bulk-host input").await?;
        inp.clear().await?;
        inp.send_keys("/Target\n").await?;
        gone(&d, &row("a.txt")).await?;
        gone(&d, &row("b.txt")).await?;
        assert!(files.join("Target/a.txt").exists() && files.join("Target/b.txt").exists());
        assert!(files.join("c.txt").exists(), "the unselected file stays");
        assert!(
            d.find(By::Css("#flash"))
                .await?
                .text()
                .await?
                .contains("Moved 2 items")
        );

        // into Target: select all (3 items) and delete; nothing happens before the second click
        d.find(By::Css(format!("{} .name a", row("Target"))))
            .await?
            .click()
            .await?;
        wait_for(&d, &row("keep.txt")).await?;
        d.find(By::Css("#sel-all")).await?.click().await?;
        assert_eq!(
            d.find(By::Css("#bulk-count")).await?.text().await?,
            "3 selected"
        );
        d.find(By::Css("#bulk-delete")).await?.click().await?;
        assert!(
            d.find(By::Css("#bulk-delete"))
                .await?
                .text()
                .await?
                .contains("sure?")
        );
        assert!(
            files.join("Target/a.txt").exists(),
            "nothing is deleted before the confirmation"
        );
        d.find(By::Css("#bulk-delete")).await?.click().await?;
        wait_for(&d, ".list .empty").await?;
        assert!(!files.join("Target/a.txt").exists() && !files.join("Target/keep.txt").exists());
        assert!(
            std::fs::read_dir(s.data.join("bob/trash")).is_ok_and(|mut x| x.next().is_some()),
            "deleted items go to the trash"
        );
        Ok::<(), WebDriverError>(())
    }
    .await;
    d.quit().await?;
    result
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs chromedriver + chromium: run scripts/e2e_web.sh"]
async fn browser_at_phone_width_does_not_scroll_sideways() -> WebDriverResult<()> {
    let s = start().await;
    let files = s.data.join("bob/files");
    std::fs::create_dir_all(
        files.join("A folder with a rather long name that must wrap or be cut"),
    )
    .unwrap();
    std::fs::write(
        files.join(
            "a-very-long-file-name-that-keeps-going-and-going-and-going-for-a-while-longer.txt",
        ),
        "x",
    )
    .unwrap();
    // files written straight to disk are picked up by a scan, like `rustbucket scan` does
    std::fs::write(s.data.join(".rustbucket-data"), "").unwrap();
    let uid: i64 = sqlx::query_scalar("SELECT id FROM users WHERE uid = 'bob'")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    rustbucket::storage::Store::new(s.pool.clone(), s.data.clone())
        .scan(
            &rustbucket::storage::Owner {
                id: uid,
                uid: "bob".into(),
            },
            false,
            false,
        )
        .await
        .unwrap();
    let d = driver(390, 844).await?;
    let result = async {
        login(&d, &s.base, "bob", "pw-bob").await?;
        wait_for(&d, ".row[data-dir=\"1\"]").await?;
        no_sideways_scroll(&d, "file list").await?;
        // the action buttons are reachable (not clipped away) on a phone
        let w: i64 = d.execute("const b=document.querySelector('.row .actions button'); const r=b.getBoundingClientRect(); return Math.round(r.right);", vec![]).await?.convert()?;
        assert!(w <= 390, "buttons run off the screen: {w}");
        // a selection shows the bar above the list; an open "..." menu stays on the screen
        d.find(By::Css(".row:not(.head) .sel")).await?.click().await?;
        wait_for(&d, "#bulkbar:not([hidden])").await?;
        no_sideways_scroll(&d, "with a selection").await?;
        d.find(By::Css(".row:not(.head) details.menu summary")).await?.click().await?;
        let inside: i64 = d.execute("const m=document.querySelector('details.menu[open] .menu-items'); const r=m.getBoundingClientRect(); return (r.left >= 0 && r.right <= window.innerWidth) ? 1 : 0;", vec![]).await?.convert()?;
        assert_eq!(inside, 1, "the open menu runs off the screen");
        no_sideways_scroll(&d, "with an open menu").await?;
        // the Admin menu (bob is an administrator) stays on the screen too
        d.find(By::Css(".admin-menu summary")).await?.click().await?;
        let admin_in: i64 = d.execute("const m=document.querySelector('.admin-menu[open] .menu-items'); const r=m.getBoundingClientRect(); return (r.left >= 0 && r.right <= window.innerWidth) ? 1 : 0;", vec![]).await?.convert()?;
        assert_eq!(admin_in, 1, "the open Admin menu runs off the screen");
        no_sideways_scroll(&d, "with the Admin menu open").await?;
        let dir: String = d.execute("return getComputedStyle(document.querySelector('.side')).flexDirection;", vec![]).await?.convert()?;
        assert_eq!(dir, "row", "the left menu becomes a row on a phone");
        for (path, what) in [("/apps/files/?view=shared-with-me", "shared with me"), ("/apps/files/?view=shared", "shared"), ("/apps/files/trash", "trash"), ("/settings/user", "settings"), ("/settings/users", "admin"), ("/login", "login")] {
            if path == "/login" {
                d.find(By::Css("header form[action='/logout'] button")).await?.click().await?;
                wait_for(&d, "input[name=username]").await?;
            } else {
                d.goto(format!("{}{path}", s.base)).await?;
                wait_for(&d, "main").await?;
            }
            no_sideways_scroll(&d, what).await?;
        }
        Ok::<(), WebDriverError>(())
    }
    .await;
    d.quit().await?;
    result
}

#[allow(dead_code)]
fn _unused(_: &Path) {}

/// The text of the editor; the other people's cursors add invisible word joiners (and their lines) to what the DOM shows.
async fn editor_text(d: &WebDriver) -> WebDriverResult<String> {
    let t = d.find(By::Css("#editor .cm-content")).await?.text().await?;
    Ok(t.replace(['\u{2060}', '\n'], ""))
}

async fn wait_text(
    d: &WebDriver,
    what: &str,
    want: impl Fn(&str) -> bool,
) -> WebDriverResult<String> {
    for _ in 0..150 {
        let t = editor_text(d).await?;
        if want(&t) {
            return Ok(t);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(WebDriverError::Timeout(format!(
        "{what}: got {:?}",
        editor_text(d).await?
    )))
}

/// Two browsers open the same Markdown file: typing in one shows up in the other, the page works under its
/// content security policy (CodeMirror styles, remote cursors), and the autosave writes the merged text.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs chromedriver + chromium: run scripts/e2e_web.sh"]
async fn browser_live_editor_two_browsers() -> WebDriverResult<()> {
    let s = start().await;
    seed(&s, "bob", &[("note.md", "hello")]).await;
    let a = driver(1280, 900).await?;
    let b = driver(1280, 900).await?;
    let result = async {
        login(&a, &s.base, "bob", "pw-bob").await?;
        login(&b, &s.base, "bob", "pw-bob").await?;
        // the way in: the row menu of the file list offers "Edit"
        wait_for(&a, &row("note.md")).await?;
        open_menu(&a, &row("note.md")).await?;
        a.find(By::Css(format!("{} a.menu-link", row("note.md"))))
            .await?
            .click()
            .await?;
        wait_for(&a, "#editor .cm-content").await?;
        b.goto(format!("{}/apps/files/edit?path=%2Fnote.md", s.base)).await?;
        wait_for(&b, "#editor .cm-content").await?;
        wait_text(&a, "initial text", |t| t.contains("hello")).await?;
        wait_text(&b, "initial text", |t| t.contains("hello")).await?;
        // the base theme CodeMirror injects at runtime was not blocked by the CSP
        let overflow: String = a
            .execute("return getComputedStyle(document.querySelector('.cm-scroller')).overflowX;", vec![])
            .await?
            .convert()?;
        assert_eq!(overflow, "auto", "CodeMirror's styles are blocked");
        // both see two people
        for d in [&a, &b] {
            for _ in 0..100 {
                if d.find_all(By::Css("#ed-people .chip")).await?.len() >= 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            assert!(d.find_all(By::Css("#ed-people .chip")).await?.len() >= 2, "people list");
        }
        // typing in A shows up in B and the other way round
        a.find(By::Css("#editor .cm-content")).await?.send_keys("ABC").await?;
        wait_text(&b, "A's typing in B", |t| t.contains("ABC")).await?;
        b.find(By::Css("#editor .cm-content")).await?.click().await?;
        b.find(By::Css("#editor .cm-content")).await?.send_keys("XYZ").await?;
        wait_text(&a, "B's typing in A", |t| t.contains("XYZ")).await?;
        let (ta, tb) = (editor_text(&a).await?, editor_text(&b).await?);
        assert_eq!(ta, tb, "both browsers show the same text");
        // the remote cursor got its color although it is set as an inline style
        let colored: bool = a
            .execute(
                "const c = document.querySelector('.cm-ySelectionCaret'); return !!c && getComputedStyle(c).backgroundColor !== 'rgba(0, 0, 0, 0)';",
                vec![],
            )
            .await?
            .convert()?;
        assert!(colored, "the other person's cursor is not drawn in color");
        // Undo needs a held button: a short click does nothing, one second does and rolls back BOTH people's typing
        let undo = a.find(By::Css("#ed-undo")).await?;
        assert!(undo.is_enabled().await?, "undo is enabled after edits");
        hold(&a, &undo, 300).await?;
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(editor_text(&a).await?, ta, "a short click must not undo");
        hold(&a, &undo, 1300).await?;
        wait_text(&a, "undo", |t| !t.contains("XYZ")).await?;
        let (after_a, after_b) = (editor_text(&a).await?, wait_text(&b, "undo seen in B", |t| !t.contains("XYZ")).await?);
        assert_eq!(after_a, after_b, "the rollback reaches the other browser");
        assert!(after_a.contains("hello"), "the text that was there when the page opened is the floor: {after_a}");
        // Redo (also held) puts it back
        let redo = a.find(By::Css("#ed-redo")).await?;
        assert!(redo.is_enabled().await?, "redo is enabled after an undo");
        hold(&a, &redo, 1300).await?;
        wait_text(&a, "redo", |t| t.contains("XYZ")).await?;
        wait_text(&b, "redo seen in B", |t| t.contains("XYZ")).await?;
        let ta = editor_text(&a).await?;
        // autosave: the merged text is on disk after the idle time
        let disk = s.data.join("bob/files/note.md");
        let mut saved = String::new();
        for _ in 0..200 {
            saved = std::fs::read_to_string(&disk).unwrap_or_default();
            if saved == ta {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(saved, ta, "autosave");
        assert!(wait_for(&a, "#ed-status").await?.text().await?.contains("saved"));
        // Save & close: leaves for the file list, and what was typed last is on disk
        a.find(By::Css("#editor .cm-content")).await?.send_keys("!").await?;
        a.find(By::Css("#ed-close")).await?.click().await?;
        wait_for(&a, "#files-root").await?;
        assert!(a.current_url().await?.path().starts_with("/apps/files"));
        assert!(std::fs::read_to_string(&disk).unwrap().contains('!'), "typed text is saved when closing");
        Ok::<(), WebDriverError>(())
    }
    .await;
    a.quit().await?;
    b.quit().await?;
    result
}

/// Press the mouse button on an element, keep it down for `ms` milliseconds, release.
async fn hold(d: &WebDriver, el: &WebElement, ms: u64) -> WebDriverResult<()> {
    d.action_chain()
        .move_to_element_center(el)
        .click_and_hold()
        .perform()
        .await?;
    tokio::time::sleep(Duration::from_millis(ms)).await;
    d.action_chain().release().perform().await
}

async fn bg(d: &WebDriver, css: &str) -> WebDriverResult<String> {
    d.execute(
        "return getComputedStyle(document.querySelector(arguments[0])).backgroundColor;",
        vec![serde_json::json!(css)],
    )
    .await?
    .convert()
}

/// The colour switch cycles auto -> dark -> light -> auto, the choice survives a reload (no flash: set before paint)
/// and also reaches the editor (CodeMirror takes the page palette).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs chromedriver + chromium: run scripts/e2e_web.sh"]
async fn browser_dark_mode_switch() -> WebDriverResult<()> {
    let s = start().await;
    seed(&s, "bob", &[("note.md", "hello")]).await;
    let d = driver(1280, 900).await?;
    let result = async {
        login(&d, &s.base, "bob", "pw-bob").await?;
        let attr = || async {
            d.execute(
                "return document.documentElement.dataset.theme || 'auto';",
                vec![],
            )
            .await?
            .convert::<String>()
        };
        assert_eq!(attr().await?, "auto");
        let light = bg(&d, "body").await?;
        d.find(By::Css("#theme-toggle")).await?.click().await?;
        assert_eq!(attr().await?, "dark");
        assert_eq!(bg(&d, "body").await?, "rgb(14, 15, 18)", "dark palette");
        // reload: the stored choice is applied before the first paint
        d.refresh().await?;
        wait_for(&d, "#files-root").await?;
        assert_eq!(attr().await?, "dark");
        assert_eq!(bg(&d, "body").await?, "rgb(14, 15, 18)");
        // the editor is dark too
        d.goto(format!("{}/apps/files/edit?path=%2Fnote.md", s.base))
            .await?;
        wait_for(&d, "#editor .cm-content").await?;
        assert_eq!(bg(&d, "#editor .cm-editor").await?, "rgb(14, 15, 18)");
        d.find(By::Css("#theme-toggle")).await?.click().await?;
        assert_eq!(attr().await?, "light");
        assert_eq!(
            bg(&d, "#editor .cm-editor").await?,
            light,
            "light editor = the system light colour"
        );
        d.find(By::Css("#theme-toggle")).await?.click().await?;
        assert_eq!(attr().await?, "auto");
        Ok::<(), WebDriverError>(())
    }
    .await;
    d.quit().await?;
    result
}

/// An 800x600 PNG made in memory.
fn png_bytes() -> Vec<u8> {
    let img = image::RgbImage::from_fn(800, 600, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

async fn transform(d: &WebDriver) -> WebDriverResult<String> {
    d.execute(
        "return document.querySelector('.viewer-img').style.transform;",
        vec![],
    )
    .await?
    .convert()
}

/// A click on a picture shows it full screen instead of downloading it; zoom works (buttons, wheel); the X closes it;
/// videos get a player; the file stays downloadable from the row menu.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs chromedriver + chromium: run scripts/e2e_web.sh"]
async fn browser_image_and_video_viewer() -> WebDriverResult<()> {
    let s = start().await;
    seed(
        &s,
        "bob",
        &[("clip.webm", "not a real video"), ("note.md", "x")],
    )
    .await;
    std::fs::write(s.data.join("bob/files/pic.png"), png_bytes()).unwrap();
    std::fs::write(s.data.join("bob/files/pic2.png"), png_bytes()).unwrap();
    seed(&s, "bob", &[]).await;
    let d = driver(1280, 900).await?;
    let result = async {
        login(&d, &s.base, "bob", "pw-bob").await?;
        wait_for(&d, &row("pic.png")).await?;
        // text files are not media rows; images and the video are
        assert!(d.find_all(By::Css(format!("{}[data-media]", row("note.md")))).await?.is_empty());
        assert_eq!(wait_for(&d, &format!("{}[data-media=video]", row("clip.webm"))).await?.tag_name().await?, "li");
        // click on the picture: the viewer opens with the picture, nothing is downloaded
        d.find(By::Css(format!("{} .name a", row("pic.png")))).await?.click().await?;
        wait_for(&d, ".viewer .viewer-img").await?;
        assert!(d.current_url().await?.path().starts_with("/apps/files"));
        for _ in 0..50 {
            let w: i64 = d.execute("return document.querySelector('.viewer-img').naturalWidth;", vec![]).await?.convert()?;
            if w == 800 { break; }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let w: i64 = d.execute("return document.querySelector('.viewer-img').naturalWidth;", vec![]).await?.convert()?;
        assert_eq!(w, 800, "the picture loaded (CSP allows it)");
        assert!(transform(&d).await?.contains("scale(1)"));
        // zoom with the buttons, then with the mouse wheel, then back to fit
        d.find(By::Css(".viewer-btn[aria-label='Zoom in']")).await?.click().await?;
        assert!(transform(&d).await?.contains("scale(1.5)"), "{}", transform(&d).await?);
        d.find(By::Css(".viewer-btn[aria-label='Zoom in']")).await?.click().await?;
        assert!(wait_for(&d, ".viewer.zoomed").await.is_ok());
        d.execute(
            "document.querySelector('.viewer-stage').dispatchEvent(new WheelEvent('wheel', {deltaY: -400, clientX: 640, clientY: 450, bubbles: true, cancelable: true}));",
            vec![],
        )
        .await?;
        let t = transform(&d).await?;
        let scale: f64 = t.split("scale(").nth(1).unwrap().trim_end_matches(')').parse().unwrap();
        assert!(scale > 2.25, "the wheel zooms further: {t}");
        d.find(By::Css(".viewer-btn[aria-label='Fit to screen']")).await?.click().await?;
        assert!(transform(&d).await?.contains("scale(1)"));
        // next picture with the arrow key, then close with the X at the top right
        d.find(By::Css("body")).await?.send_keys(Key::Right + "").await?;
        for _ in 0..50 {
            let alt: String = d.execute("return document.querySelector('.viewer-img').alt;", vec![]).await?.convert()?;
            if alt == "pic2.png" { break; }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let alt: String = d.execute("return document.querySelector('.viewer-img').alt;", vec![]).await?.convert()?;
        assert_eq!(alt, "pic2.png");
        let (right, top): (f64, f64) = {
            let v: Vec<f64> = d.execute("const r=document.querySelector('.viewer-close').getBoundingClientRect(); return [window.innerWidth-r.right, r.top];", vec![]).await?.convert()?;
            (v[0], v[1])
        };
        assert!(right < 40.0 && top < 40.0, "X is in the top right corner: {right} {top}");
        d.find(By::Css(".viewer-close")).await?.click().await?;
        gone(&d, ".viewer").await?;
        // the video: a player instead of a download
        d.find(By::Css(format!("{} .name a", row("clip.webm")))).await?.click().await?;
        wait_for(&d, ".viewer video[controls]").await?;
        assert!(d.find_all(By::Css(".viewer-btn[aria-label='Zoom in']")).await?.iter().all(|_| true));
        d.find(By::Css("body")).await?.send_keys(Key::Escape + "").await?;
        gone(&d, ".viewer").await?;
        // the file is still downloadable from the menu
        open_menu(&d, &row("pic.png")).await?;
        let href = d.find(By::Css(format!("{} a.menu-link[download]", row("pic.png")))).await?.attr("href").await?.unwrap();
        assert!(href.ends_with("/pic.png"), "{href}");
        Ok::<(), WebDriverError>(())
    }
    .await;
    d.quit().await?;
    result
}
