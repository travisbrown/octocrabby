pub mod cli;
pub mod models;

use futures::stream::{self, LocalBoxStream, Stream, StreamExt, TryStreamExt};
use futures::{future, Future, FutureExt};
use itertools::Itertools;
use octocrab::{
    models::{pulls::PullRequest, User},
    Octocrab, Page,
};
use reqwest::{Response, StatusCode};
use serde::{de::DeserializeOwned, Deserialize};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::pin::Pin;

const PULL_REQUESTS_PAGE_SIZE: u8 = 100;
const FOLLOWERS_PAGE_SIZE: u8 = 100;
const FOLLOWING_PAGE_SIZE: u8 = 100;
const BLOCKS_PAGE_SIZE: u8 = 100;
const BLOCK_304_MESSAGE: &str = "Blocked user has already been blocked";
const BLOCK_404_MESSAGE: &str = "Not Found";

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
    usernames: &[&str],
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
    // TODO: Use `into_values` here when #75294 is out of nightly.
    Ok(results?.data.values().flatten().cloned().collect())
}

pub fn get_users_info_chunked<'a>(
    instance: &'a Octocrab,
    usernames: &'a [&'a str],
    chunk_size: usize,
) -> impl Stream<Item = octocrab::Result<models::UserInfo>> + 'a {
    stream::iter(usernames.chunks(chunk_size).map(Ok))
        .and_then(move |chunk| get_users_info(instance, chunk))
        .and_then(|infos| future::ok(stream::iter(infos.into_iter().map(Ok))))
        .try_flatten()
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

pub enum BlockStatus {
    NewlyBlocked,
    AlreadyBlocked,
    UserNotFound,
    OtherSuccess(StatusCode),
    OtherNonSuccess(String),
}

impl BlockStatus {
    fn from_status_code_result(
        status_code_result: octocrab::Result<StatusCodeWrapper>,
    ) -> octocrab::Result<Self> {
        match status_code_result {
            Ok(StatusCodeWrapper(status_code)) if status_code == StatusCode::NO_CONTENT => {
                Ok(BlockStatus::NewlyBlocked)
            }
            Ok(StatusCodeWrapper(status_code)) => Ok(BlockStatus::OtherSuccess(status_code)),
            Err(octocrab::Error::GitHub { source, .. }) if source.errors.is_none() => {
                Ok(if source.message.contains(BLOCK_304_MESSAGE) {
                    BlockStatus::AlreadyBlocked
                } else if source.message.contains(BLOCK_404_MESSAGE) {
                    BlockStatus::UserNotFound
                } else {
                    BlockStatus::OtherNonSuccess(source.message)
                })
            }
            Err(other) => Err(other),
        }
    }
}

/// Block a user from either an organization or a user account
pub async fn block_user(
    instance: &Octocrab,
    organization: Option<&str>,
    username: &str,
) -> octocrab::Result<BlockStatus> {
    match organization {
        Some(value) => block_user_for_organization(instance, value, username).await,
        None => block_user_for_user(instance, username).await,
    }
}

/// Block a user and indicate the result of the operation
pub async fn block_user_for_user(
    instance: &Octocrab,
    username: &str,
) -> octocrab::Result<BlockStatus> {
    let route = format!("/user/blocks/{}", username);

    BlockStatus::from_status_code_result(
        instance.put::<StatusCodeWrapper, _, ()>(route, None).await,
    )
}

/// Block a user from an organization
pub async fn block_user_for_organization(
    instance: &Octocrab,
    organization: &str,
    username: &str,
) -> octocrab::Result<BlockStatus> {
    let route = format!("/orgs/{}/blocks/{}", organization, username);

    BlockStatus::from_status_code_result(
        instance.put::<StatusCodeWrapper, _, ()>(route, None).await,
    )
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

pub fn get_blocks<'a>(
    instance: &'a Octocrab,
    organization: Option<&'a str>,
) -> LocalBoxStream<'a, octocrab::Result<User>> {
    match organization {
        Some(value) => Box::pin(get_blocks_for_organization(instance, value)),
        None => Box::pin(get_blocks_for_user(instance)),
    }
}

pub fn get_blocks_for_user(instance: &Octocrab) -> impl Stream<Item = octocrab::Result<User>> + '_ {
    let route = "user/blocks";
    let opts = vec![("per_page", BLOCKS_PAGE_SIZE)];

    stream::once(async move { instance.get::<Page<User>, _, _>(route, Some(&opts)).await })
        .and_then(move |page| future::ok(pager_stream(&instance, page)))
        .try_flatten()
}

pub fn get_blocks_for_organization<'a>(
    instance: &'a Octocrab,
    organization: &'a str,
) -> impl Stream<Item = octocrab::Result<User>> + 'a {
    let route = format!("orgs/{}/blocks", organization);
    let opts = vec![("per_page", BLOCKS_PAGE_SIZE)];

    stream::once(async move { instance.get::<Page<User>, _, _>(route, Some(&opts)).await })
        .and_then(move |page| future::ok(pager_stream(&instance, page)))
        .try_flatten()
}

#[derive(Default)]
pub struct Exclusions(HashMap<String, HashSet<String>>);

impl Exclusions {
    pub fn load<R: Read>(reader: R) -> csv::Result<Exclusions> {
        let mut csv_reader = csv::ReaderBuilder::new()
            .has_headers(false)
            .from_reader(reader);
        let mut pairs = csv_reader
            .deserialize::<(String, String)>()
            .collect::<csv::Result<Vec<_>>>()?;
        pairs.sort_unstable_by(|(repo1, _), (repo2, _)| repo1.cmp(repo2));

        Ok(Exclusions(
            pairs
                .into_iter()
                .group_by(|(repo, _)| repo.clone())
                .into_iter()
                .map(|(repo, pairs)| {
                    (
                        repo,
                        pairs.map(|(_, username)| username.to_lowercase()).collect(),
                    )
                })
                .collect(),
        ))
    }

    pub fn is_excluded(&self, repo: &str, username: &str) -> bool {
        // Only accounts that are treated specially by GitHub should be hard-coded here
        // All other exclusions should be managed with an exclusions file
        username == "ghost"
            || username == "dependabot[bot]"
            || self.0.get(repo).map_or(false, |usernames| {
                usernames.contains(&username.to_lowercase())
            })
    }
}
