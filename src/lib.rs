pub mod cli;
pub mod models;

use futures::stream::{self, Stream, StreamExt, TryStreamExt};
use futures::{future, Future, FutureExt};
use octocrab::{
    models::{pulls::PullRequest, User},
    Octocrab, Page,
};
use reqwest::{Response, StatusCode};
use serde::{de::DeserializeOwned, Deserialize};
use std::collections::HashMap;
use std::pin::Pin;

const PULL_REQUESTS_PAGE_SIZE: u8 = 100;
const FOLLOWERS_PAGE_SIZE: u8 = 100;
const FOLLOWING_PAGE_SIZE: u8 = 100;
const BLOCKS_PAGE_SIZE: u8 = 100;
const BLOCK_304_MESSAGE: &str = "Blocked user has already been blocked";

/// Initialize a client instance with defaults and configuration
pub fn init(token: Option<String>) -> octocrab::Result<Octocrab> {
    let builder = octocrab::OctocrabBuilder::new();

    match token {
        Some(value) => builder.personal_token(value).build(),
        None => builder.build(),
    }
}

/// Parse a repo "path" (e.g. "travisbrown/octocrabby")
pub fn parse_repo_path(path: &str) -> Option<(&str, &str)> {
    let parts = path.split('/').collect::<Vec<_>>();

    if parts.len() == 2 {
        Some((parts[0], parts[1]))
    } else {
        None
    }
}

/// Asynchronously stream results for a starting page
pub fn pager_stream<'a, R: DeserializeOwned + 'a>(
    instance: &'a Octocrab,
    start: Page<R>,
) -> impl Stream<Item = octocrab::Result<R>> + 'a {
    stream::try_unfold(Some(start), move |current| async move {
        match current {
            Some(current_page) => instance
                .get_page::<R>(&current_page.next)
                .await
                .map(|next| Some((current_page, next))),
            None => Ok(None),
        }
    })
    .and_then(|mut page| future::ok(stream::iter(page.take_items()).map(Ok)))
    .try_flatten()
}

/// Stream pull requests for a repo
pub fn pull_requests<'a>(
    instance: &'a Octocrab,
    owner: &'a str,
    repo: &'a str,
) -> impl Stream<Item = octocrab::Result<PullRequest>> + 'a {
    stream::once(async move {
        instance
            .pulls(owner, repo)
            .list()
            .state(octocrab::params::State::All)
            .per_page(PULL_REQUESTS_PAGE_SIZE)
            .send()
            .await
    })
    .and_then(move |page| future::ok(pager_stream(&instance, page)))
    .try_flatten()
}

struct StatusCodeWrapper(StatusCode);

impl octocrab::FromResponse for StatusCodeWrapper {
    fn from_response<'a>(
        response: Response,
    ) -> Pin<Box<dyn Future<Output = octocrab::Result<Self>> + Send + 'a>> {
        future::ok(StatusCodeWrapper(response.status())).boxed()
    }
}

/// Check whether one user follows another
pub async fn check_follow(
    instance: &Octocrab,
    source: &str,
    target: &str,
) -> octocrab::Result<bool> {
    let route = format!("/users/{}/following/{}", source, target);

    match instance.get::<StatusCodeWrapper, _, ()>(route, None).await {
        Ok(StatusCodeWrapper(status_code)) => Ok(status_code == StatusCode::NO_CONTENT),
        Err(octocrab::Error::GitHub { source, .. }) if source.errors.is_none() => Ok(false),
        Err(other) => Err(other),
    }
}

#[derive(Deserialize)]
struct GraphQlUserResults {
    data: HashMap<String, Option<models::UserInfo>>,
}

pub async fn get_users_info(
    instance: &Octocrab,
    usernames: &[String],
) -> octocrab::Result<Vec<models::UserInfo>> {
    let user_aliases = usernames
        .iter()
        .enumerate()
        .map(|(i, username)| format!("u{}: user(login: \"{}\") {{ ...UserFields }}", i, username))
        .collect::<Vec<_>>()
        .join("\n");

    let query = format!(
        "query {{{}}}\nfragment UserFields on User {{ login\ncreatedAt\nname\ntwitterUsername }}",
        user_aliases
    );

    let results: octocrab::Result<GraphQlUserResults> = instance.graphql(&query).await;
    Ok(results?.data.values().flatten().cloned().collect())
}

/// Get extended information for a user
pub async fn get_user(
    instance: &Octocrab,
    username: &str,
) -> octocrab::Result<models::ExtendedUser> {
    let route = format!("/users/{}", username);

    instance
        .get::<models::ExtendedUser, _, ()>(route, None)
        .await
}

/// Block a user and indicate whether this operation changed their block status
pub async fn block_user(instance: &Octocrab, username: &str) -> octocrab::Result<bool> {
    let route = format!("/user/blocks/{}", username);

    match instance.put::<StatusCodeWrapper, _, ()>(route, None).await {
        Ok(StatusCodeWrapper(status_code)) => Ok(status_code == StatusCode::NO_CONTENT),
        Err(octocrab::Error::GitHub { source, .. })
            if source.message.contains(BLOCK_304_MESSAGE) =>
        {
            Ok(false)
        }
        Err(other) => Err(other),
    }
}

pub fn get_followers(instance: &Octocrab) -> impl Stream<Item = octocrab::Result<User>> + '_ {
    let route = "user/followers";
    let opts = vec![("per_page", FOLLOWERS_PAGE_SIZE)];

    stream::once(async move { instance.get::<Page<User>, _, _>(route, Some(&opts)).await })
        .and_then(move |page| future::ok(pager_stream(&instance, page)))
        .try_flatten()
}

pub fn get_following(instance: &Octocrab) -> impl Stream<Item = octocrab::Result<User>> + '_ {
    let route = "user/following";
    let opts = vec![("per_page", FOLLOWING_PAGE_SIZE)];

    stream::once(async move { instance.get::<Page<User>, _, _>(route, Some(&opts)).await })
        .and_then(move |page| future::ok(pager_stream(&instance, page)))
        .try_flatten()
}

pub fn get_blocks(instance: &Octocrab) -> impl Stream<Item = octocrab::Result<User>> + '_ {
    let route = "user/blocks";
    let opts = vec![("per_page", BLOCKS_PAGE_SIZE)];

    stream::once(async move { instance.get::<Page<User>, _, _>(route, Some(&opts)).await })
        .and_then(move |page| future::ok(pager_stream(&instance, page)))
        .try_flatten()
}
