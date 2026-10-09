//! Requests with credentials (lfsapi/auth.go): the access mode of the endpoint decides
//! whether credentials are asked for; a 401 upgrades it (from Lfs-Authenticate or
//! Www-Authenticate) and the request is sent again, at most three times.

use crate::creds::{self, Wrapper};
use crate::endpoint::{self, Access};
use crate::errors::{Error, Kind, Result};
use crate::gourl;
use crate::http::{self, Hooks, Redirect, Request, Response};
use std::sync::Mutex;

pub type Outcome = (Option<Response>, Option<Error>);

static MODES: Mutex<Option<Vec<Access>>> = Mutex::new(None);

fn modes() -> Vec<Access> {
    MODES.lock().unwrap().get_or_insert_with(|| vec![Access::None, Access::Negotiate, Access::Basic]).clone()
}

pub fn req_operation(req: &Request) -> &'static str {
    if req.method == "POST" || req.method == "PUT" {
        "upload"
    } else {
        "download"
    }
}

/// DoAPIRequestWithAuth: with the access of the remote's API endpoint.
pub fn do_api_request_with_auth(remote: &str, req: &mut Request, hooks: &mut Hooks) -> Outcome {
    let ep = endpoint::endpoint(req_operation(req), remote);
    let access = endpoint::access_for(&ep.url);
    do_with_auth(remote, access, req, hooks)
}

/// DoWithAuth: sent again while the server asks for (other) credentials.
pub fn do_with_auth(remote: &str, mut access: (Access, String), req: &mut Request, hooks: &mut Hooks) -> Outcome {
    let max = if access.0 == Access::None { 4 } else { 3 };
    for i in 0..max {
        let (res, err) = do_with_auth_once(remote, access.clone(), req, hooks);
        if !err.as_ref().is_some_and(|e| e.is(Kind::Auth)) {
            return (res, err);
        }
        if req.header.has("Authorization") {
            return (res, err);
        }
        if i < max - 1 {
            access = endpoint::access_for(&access.1);
            crate::trace!("api: http response indicates {} authentication. Resubmitting...", crate::tools::quote(access.0.mode()));
        }
    }
    creds::set_state_fields(vec![]);
    crate::trace!("api: too many authentication attempts");
    (None, Some(Error::new("too many authentication attempts")))
}

/// DoWithAuthNoRetry.
pub fn do_with_auth_no_retry(remote: &str, access: (Access, String), req: &mut Request, hooks: &mut Hooks) -> Outcome {
    do_with_auth_once(remote, access, req, hooks)
}

fn do_with_auth_once(remote: &str, access: (Access, String), req: &mut Request, hooks: &mut Hooks) -> Outcome {
    let cl = http::client();
    cl.apply_extra_headers(req);
    let wrapper = match get_creds(remote, &access, req) {
        Ok(w) => w,
        Err(e) => return (None, Some(e)),
    };
    creds::set_state_fields(wrapper.creds.as_ref().and_then(|c| c.get("state[]").cloned()).unwrap_or_default());
    let (res, err) = do_with_creds(req, &access, hooks);
    if err.as_ref().is_some_and(|e| e.is(Kind::Auth)) {
        let multistage = wrapper.creds.as_ref().is_some_and(creds::is_multistage);
        let (new_mode, new_modes, headers) = get_auth_access(res.as_ref(), access.0, &modes(), multistage);
        if new_mode != access.0 {
            endpoint::set_access(&access.1, new_mode);
            *MODES.lock().unwrap() = Some(new_modes);
        }
        if wrapper.creds.is_some() {
            req.header.del("Authorization");
            if !multistage {
                wrapper.reject();
            }
        }
        creds::set_www_auth_headers(headers);
    }
    if let Some(r) = &res {
        if (200..300).contains(&r.status) {
            wrapper.approve();
        }
    }
    (res, err)
}

fn do_with_creds(req: &Request, access: &(Access, String), hooks: &mut Hooks) -> Outcome {
    let cl = http::client();
    let mut r = req.clone();
    let mut via = 0;
    loop {
        r.header.set("User-Agent", &http::user_agent());
        match cl.do_with_redirect(&r, &mut via, hooks) {
            Redirect::Done(res, err) => return (res, err),
            Redirect::To(next) => {
                // doWithAuth("", access, redirectedReq): credentials for the new location.
                r = next;
                cl.apply_extra_headers(&mut r);
                let w = match get_creds("", access, &mut r) {
                    Ok(w) => w,
                    Err(e) => return (None, Some(e)),
                };
                let _ = w;
            }
        }
    }
}

fn get_creds(remote: &str, access: &(Access, String), req: &mut Request) -> Result<Wrapper> {
    let operation = req_operation(req);
    let api = endpoint::endpoint(operation, remote);
    if access.0 != Access::Negotiate {
        if request_has_auth(req) || access.0 == Access::None {
            return Ok(creds::null_wrapper());
        }
        let creds_url = match cred_url_for_api(operation, remote, &api.url, req) {
            Ok(Some(u)) => u,
            Ok(None) => return Ok(creds::null_wrapper()),
            Err(e) => return Err(e.wrap("credentials")),
        };
        let mut w = creds::wrapper_for(&creds_url);
        w.fill_creds()?;
        crate::trace!("Filled credentials for {}", creds_url.to_string_go());
        set_request_auth_with_creds(req, w.creds.as_ref().unwrap());
        return Ok(w);
    }
    let u = gourl::parse(&api.url).map_err(|e| Error::new(e.to_string()).wrap("credentials"))?;
    Ok(creds::wrapper_for(&u))
}

fn cred_url_for_api(operation: &str, remote: &str, api_url: &str, req: &mut Request) -> Result<Option<gourl::Url>> {
    let api = gourl::parse(api_url).map_err(|e| Error::new(e.to_string()))?;
    let ru = req.parsed_url();
    if ru.scheme != api.scheme || ru.host != api.host {
        return Ok(Some(ru));
    }
    if set_request_auth_from_url(req, &api) {
        return Ok(None);
    }
    if !remote.is_empty() {
        let u = endpoint::git_remote_url(remote, operation == "upload");
        if !u.is_empty() {
            let schemed = fix_schemeless_url(&u);
            let g = gourl::parse(&schemed).map_err(|e| Error::new(e.to_string()))?;
            if g.scheme == api.scheme && g.host == api.host {
                if set_request_auth_from_url(req, &g) {
                    return Ok(None);
                }
                return Ok(Some(g));
            }
        }
    }
    Ok(Some(api))
}

fn fix_schemeless_url(u: &str) -> String {
    if ["ssh://", "http://", "https://"].iter().any(|s| u.starts_with(s)) {
        return u.to_string();
    }
    let colon = u.find(':');
    let slash = u.find('/');
    match (colon, slash) {
        (Some(c), Some(s)) if c > s => u.to_string(),
        (Some(_), _) => format!("//{}", u.replacen(':', "/", 1)),
        _ => u.to_string(),
    }
}

fn request_has_auth(req: &Request) -> bool {
    if !req.header.get("Authorization").is_empty() {
        return true;
    }
    let u = req.parsed_url();
    u.raw_query.as_deref().is_some_and(|q| q.split('&').any(|kv| kv.split_once('=').is_some_and(|(k, v)| k == "token" && !v.is_empty())))
}

fn set_request_auth_from_url(req: &mut Request, u: &gourl::Url) -> bool {
    if let Some((user, Some(pass))) = &u.user {
        eprintln!("warning: current Git remote contains credentials");
        set_request_auth(req, user, pass);
        return true;
    }
    false
}

fn set_request_auth(req: &mut Request, user: &str, pass: &str) {
    if user.is_empty() && pass.is_empty() {
        return;
    }
    let token = crate::tools::base64(format!("{user}:{pass}").as_bytes());
    req.header.set("Authorization", &format!("Basic {}", token.trim()));
}

fn set_request_auth_with_creds(req: &mut Request, c: &creds::Creds) {
    let authtype = creds::first(c, "authtype");
    let credential = creds::first(c, "credential");
    if authtype.is_empty() && credential.is_empty() {
        set_request_auth(req, &creds::first(c, "username"), &creds::first(c, "password"));
        return;
    }
    req.header.set("Authorization", &format!("{authtype} {credential}"));
}

fn get_auth_access(res: Option<&Response>, access: Access, modes: &[Access], multistage: bool) -> (Access, Vec<Access>, Vec<String>) {
    let new_modes: Vec<Access> = modes.iter().copied().filter(|m| multistage || *m != access).collect();
    let mut headers = vec![];
    if let Some(res) = res {
        for h in ["Lfs-Authenticate", "Www-Authenticate"] {
            headers.extend(res.header.values(h));
        }
        let mut supported = vec![];
        for h in ["Lfs-Authenticate", "Www-Authenticate"] {
            for a in res.header.values(h) {
                let m = a.to_lowercase();
                supported.push(m.split(' ').next().unwrap_or("").to_string());
            }
        }
        for m in &new_modes {
            if supported.iter().any(|s| s == m.mode()) {
                return (*m, new_modes.clone(), headers);
            }
        }
    }
    (Access::Basic, new_modes, headers)
}
