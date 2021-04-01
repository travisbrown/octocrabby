use clap::{crate_authors, crate_version, Clap};
use futures::{future, stream::TryStreamExt};
use itertools::Itertools;
use octocrab::Octocrab;
use octocrabby::{
    block_user, check_follow, cli, get_blocks, models::UserInfo, parse_repo_path, pull_requests,
    BlockStatus, Exclusions,
};
use std::collections::{HashMap, HashSet};
use std::default::Default;
use std::fs::File;

type Void = Result<(), Box<dyn std::error::Error>>;

#[tokio::main]
async fn main() -> Void {
    let opts: Opts = Opts::parse();
    let _ = cli::init_logging(opts.verbose);
    let instance = octocrabby::init(opts.token)?;

    match opts.command {
        Command::BlockUsers { org, force } => {
            // Note that only the first field is used, and is expected to be a GitHub login username
            let mut reader = csv::ReaderBuilder::new()
                .has_headers(false)
                .from_reader(std::io::stdin());
            let mut usernames = vec![];

            for record in reader.records() {
                usernames.push(record?.get(0).unwrap().to_string());
            }

            if !force {
                let known: HashSet<String> = octocrabby::get_blocks(&instance, org.as_deref())
                    .and_then(|user| future::ok(user.login))
                    .try_collect()
                    .await?;

                let unfiltered_size = usernames.len();

                usernames.retain(|username| !known.contains(username));

                log::warn!(
                    "Skipping {} known blocked users",
                    unfiltered_size - usernames.len()
                );
            }

            for username in usernames {
                match block_user(&instance, org.as_deref(), &username).await? {
                    BlockStatus::NewlyBlocked => log::info!("Successfully blocked {}", username),
                    BlockStatus::AlreadyBlocked => log::warn!("{} was already blocked", username),
                    BlockStatus::UserNotFound => log::warn!("{} was not found", username),
                    BlockStatus::OtherSuccess(status_code) => {
                        log::error!("Unknown success status code: {:?}", status_code)
                    }
                    BlockStatus::OtherNonSuccess(message) => {
                        log::error!("Unknown non-success message: {}", message)
                    }
                };
            }
        }
        Command::ListFollowers => {
            octocrabby::get_followers(&instance)
                .try_for_each(|user| {
                    println!("{},{}", user.login, user.id);
                    future::ok(())
                })
                .await?
        }
        Command::ListFollowing => {
            octocrabby::get_following(&instance)
                .try_for_each(|user| {
                    println!("{},{}", user.login, user.id);
                    future::ok(())
                })
                .await?
        }
        Command::ListBlocks { org } => {
            get_blocks(&instance, org.as_deref())
                .try_for_each(|user| {
                    println!("{},{}", user.login, user.id);
                    future::ok(())
                })
                .await?
        }
        Command::ListPrContributors {
            repo_path,
            omit_twitter,
            exclusions_file,
            ignore_exclusions,
        } => {
            if let Some((owner, repo)) = parse_repo_path(&repo_path) {
                let exclusions = if ignore_exclusions {
                    Exclusions::default()
                } else {
                    let file = File::open(exclusions_file)?;
                    Exclusions::load(file)?
                };

                log::info!("Loading pull requests");
                let mut prs = pull_requests(&instance, owner, repo)
                    .try_collect::<Vec<_>>()
                    .await?;
                prs.sort_unstable_by(|pr1, pr2| pr1.user.login.cmp(&pr2.user.login));

                let by_username = prs
                    .into_iter()
                    .group_by(|pr| (pr.user.login.clone(), pr.user.id));

                let results = by_username
                    .into_iter()
                    .map(|((username, user_id), prs)| {
                        let batch = prs.collect::<Vec<_>>();
                        (
                            username,
                            user_id,
                            batch.len(),
                            batch.into_iter().map(|pr| pr.created_at).min().unwrap(),
                        )
                    })
                    .collect::<Vec<_>>();

                let usernames = results
                    .iter()
                    .map(|(username, _, _, _)| username.as_str())
                    .collect::<Vec<_>>();

                // Load additional information that's only available if you're authenticated
                let mut additional_info: Option<AdditionalUserInfo> =
                    if instance.current().user().await.is_ok() {
                        Some(load_additional_user_info(&instance, &usernames).await?)
                    } else {
                        None
                    };

                let mut writer = csv::Writer::from_writer(std::io::stdout());

                for (username, user_id, pr_count, first_pr_date) in results {
                    if exclusions.is_excluded(&repo_path, &username) {
                        log::warn!("Excluded user {}", username);
                    } else {
                        let mut record =
                            vec![username.clone(), user_id.to_string(), pr_count.to_string()];

                        // Add other fields to the record if you're authenticated
                        if let Some(AdditionalUserInfo {
                            ref follows_you,
                            ref you_follow,
                            ref mut user_info,
                        }) = additional_info
                        {
                            let (age, name, twitter_username) = match user_info.remove(&username) {
                                Some(info) => (
                                    (first_pr_date - info.created_at).num_days(),
                                    info.name.unwrap_or_default(),
                                    info.twitter_username.unwrap_or_default(),
                                ),
                                None => {
                                    // These values will be used for accounts such as dependabot
                                    (-1, "".to_string(), "".to_string())
                                }
                            };

                            record.push(age.to_string());
                            record.push(name);
                            if !omit_twitter {
                                record.push(twitter_username);
                            }
                            record.push(you_follow.contains(&username).to_string());
                            record.push(follows_you.contains(&username).to_string());
                        }

                        writer.write_record(&record)?;
                    }
                }
            } else {
                log::error!("Invalid repository path: {}", repo_path);
            }
        }
        Command::CheckFollow { user, follower } => {
            let target_user = match user {
                Some(value) => value,
                None => instance.current().user().await?.login,
            };

            let result = check_follow(&instance, &follower, &target_user).await?;

            println!("{}", result);
        }
    }

    Ok(())
}

#[derive(Clap)]
#[clap(name = "crabby", version = crate_version!(), author = crate_authors!())]
struct Opts {
    /// A GitHub personal access token (not needed for all operations)
    #[clap(short, long)]
    token: Option<String>,
    #[clap(short, long, parse(from_occurrences))]
    /// Logging verbosity
    verbose: i32,
    #[clap(subcommand)]
    command: Command,
}

#[derive(Clap)]
enum Command {
    /// Block a list of users provided in CSV format to stdin
    BlockUsers {
        /// The organization to block users from (instead of the authenticated user)
        #[clap(long)]
        org: Option<String>,
        /// Force block requests for all provided accounts (skip checking current block list)
        #[clap(long)]
        force: bool,
    },
    /// List the authenticated user's followers in CSV format to stdout
    ListFollowers,
    /// List accounts the authenticated user follows in CSV format to stdout
    ListFollowing,
    /// List accounts the authenticated user blocks in CSV format to stdout
    ListBlocks {
        /// The organization to list blocks for (instead of the authenticated user)
        #[clap(long)]
        org: Option<String>,
    },
    /// List PR contributors for the given repository
    ListPrContributors {
        /// The repository to check for pull requests
        #[clap(short, long)]
        repo_path: String,
        /// Omit Twitter handle (which is not verified)
        #[clap(long)]
        omit_twitter: bool,
        /// Exclusions file
        #[clap(short, long, default_value = "data/exclusions.csv")]
        exclusions_file: String,
        /// Ignore exclusions
        #[clap(long)]
        ignore_exclusions: bool,
    },
    /// Check whether one user follows another
    CheckFollow {
        /// The possibly followed user
        #[clap(short, long)]
        user: Option<String>,
        /// The possible follower
        #[clap(short, long)]
        follower: String,
    },
}

struct AdditionalUserInfo {
    follows_you: HashSet<String>,
    you_follow: HashSet<String>,
    user_info: HashMap<String, UserInfo>,
}

async fn load_additional_user_info(
    instance: &Octocrab,
    usernames: &[&str],
) -> octocrab::Result<AdditionalUserInfo> {
    log::info!("Loading follower information");
    let follows_you = octocrabby::get_followers(&instance)
        .and_then(|user| future::ok(user.login))
        .try_collect()
        .await?;

    log::info!("Loading following information");
    let you_follow = octocrabby::get_following(&instance)
        .and_then(|user| future::ok(user.login))
        .try_collect()
        .await?;

    log::info!(
        "Loading additional user information for {} users",
        usernames.len()
    );
    let user_info: HashMap<String, UserInfo> = octocrabby::get_users_info(&instance, usernames)
        .await?
        .into_iter()
        .map(|info| (info.login.clone(), info))
        .collect();

    Ok(AdditionalUserInfo {
        follows_you,
        you_follow,
        user_info,
    })
}
