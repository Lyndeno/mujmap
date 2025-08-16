use crate::args::Args;
use crate::cache::{self, Cache};
use crate::remote::{self, Remote};
use crate::{config::Config, local::Local};
use crate::{jmap, local};
use atty::Stream;
use fslock::LockFile;
use indicatif::ProgressBar;
use log::{debug, error, warn};
use rayon::{prelude::*, ThreadPoolBuildError};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Write;
use std::io::{self, BufReader, BufWriter};
use std::path::{Path, PathBuf};
use symlink::symlink_file;
use termcolor::{ColorSpec, StandardStream, WriteColor};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Could not open lock file `{}': {}", path.to_string_lossy(), source)]
    OpenLockFile { path: PathBuf, source: io::Error },

    #[error("Could not lock: {}", source)]
    Lock { source: io::Error },

    #[error("Could not log string: {}", source)]
    Log { source: io::Error },

    #[error("Could not read mujmap state file `{}': {}", filename.to_string_lossy(), source)]
    ReadStateFile {
        filename: PathBuf,
        source: io::Error,
    },

    #[error("Could not parse mujmap state file `{}': {}", filename.to_string_lossy(), source)]
    ParseStateFile {
        filename: PathBuf,
        source: serde_json::Error,
    },

    #[error("Could not create mujmap state file `{}': {}", filename.to_string_lossy(), source)]
    CreateStateFile {
        filename: PathBuf,
        source: io::Error,
    },

    #[error("Could not write to mujmap state file `{}': {}", filename.to_string_lossy(), source)]
    WriteStateFile {
        filename: PathBuf,
        source: serde_json::Error,
    },

    #[error("Could not open local database: {}", source)]
    OpenLocal { source: local::Error },

    #[error("Could not open local cache: {}", source)]
    OpenCache { source: cache::Error },

    #[error("Could not open remote session: {}", source)]
    OpenRemote { source: remote::Error },

    #[error("Could not index mailboxes: {}", source)]
    IndexMailboxes { source: remote::Error },

    #[error("JMAP server is missing mailboxes for these tags: {:?}", tags)]
    MissingMailboxes { tags: Vec<String> },

    #[error("Could not create missing mailboxes for tags `{:?}': {}", tags, source)]
    CreateMailboxes {
        tags: Vec<String>,
        source: remote::Error,
    },

    #[error("Could not index notmuch tags: {}", source)]
    IndexTags { source: notmuch::Error },

    #[error("Could not index local emails: {}", source)]
    IndexLocalEmails { source: local::Error },

    #[error("Could not index all remote email IDs for a full sync: {}", source)]
    IndexRemoteEmails { source: remote::Error },

    #[error("Could not retrieve email properties from remote: {}", source)]
    GetRemoteEmails { source: remote::Error },

    #[error("Could not create download thread pool: {}", source)]
    CreateDownloadThreadPool { source: ThreadPoolBuildError },

    #[error("Could not download email from remote: {}", source)]
    DownloadRemoteEmail { source: remote::Error },

    #[error("Could not save email to cache: {}", source)]
    CacheNewEmail { source: cache::Error },

    #[error("Missing last notmuch database revision")]
    MissingNotmuchDatabaseRevision {},

    #[error("Could not index local updated emails: {}", source)]
    IndexLocalUpdatedEmails { source: local::Error },

    #[error("Could not add new local email `{}': {}", filename.to_string_lossy(), source)]
    AddLocalEmail {
        filename: PathBuf,
        source: notmuch::Error,
    },

    #[error("Could not update local email: {}", source)]
    UpdateLocalEmail { source: notmuch::Error },

    #[error("Could not remove local email: {}", source)]
    RemoveLocalEmail { source: notmuch::Error },

    #[error("Could not get local message from notmuch: {}", source)]
    GetNotmuchMessage { source: notmuch::Error },

    #[error(
        "Could not remove unindexed mail file `{}': {}",
        path.to_string_lossy(),
        source
    )]
    RemoveUnindexedMailFile { path: PathBuf, source: io::Error },

    #[error(
        "Could not make symlink from cache `{}' to maildir `{}': {}",
        from.to_string_lossy(),
        to.to_string_lossy(),
        source
    )]
    MakeMaildirSymlink {
        from: PathBuf,
        to: PathBuf,
        source: io::Error,
    },

    #[error("Could not rename mail file from `{}' to `{}': {}", from.to_string_lossy(), to.to_string_lossy(), source)]
    RenameMailFile {
        from: PathBuf,
        to: PathBuf,
        source: io::Error,
    },

    #[error("Could not remove mail file `{}': {}", path.to_string_lossy(), source)]
    RemoveMailFile { path: PathBuf, source: io::Error },

    #[error("Could not begin atomic database operation: {}", source)]
    BeginAtomic { source: notmuch::Error },

    #[error("Could not end atomic database operation: {}", source)]
    EndAtomic { source: notmuch::Error },

    #[error("Could not push changes to JMAP server: {}", source)]
    PushChanges { source: remote::Error },

    #[error("Programmer error!")]
    Programmer {},
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A new email to be eventually added to the maildir.
#[derive(Debug)]
pub struct NewEmail<'a> {
    pub remote_email: &'a remote::Email,
    pub cache_path: PathBuf,
    pub maildir_path: PathBuf,
}

#[derive(Serialize, Deserialize)]
pub struct LatestState {
    /// Latest revision of the notmuch database since the last time mujmap was run.
    pub notmuch_revision: Option<u64>,
    /// Latest JMAP Email state returned by `Email/get`.
    pub jmap_state: Option<jmap::State>,
}

impl LatestState {
    fn open(filename: impl AsRef<Path>) -> Result<Self> {
        let filename = filename.as_ref();
        let file = File::open(filename).map_err(|source| Error::ReadStateFile { filename: filename.into(), source })?;
        let reader = BufReader::new(file);
        serde_json::from_reader(reader).map_err(|source| Error::ParseStateFile { filename: filename.into(), source })
    }

    fn save(&self, filename: impl AsRef<Path>) -> Result<()> {
        let filename = filename.as_ref();
        let file = File::create(filename).map_err(|source| Error::CreateStateFile { filename: filename.into(), source })?;
        let writer = BufWriter::new(file);
        serde_json::to_writer(writer, self).map_err(|source| Error::WriteStateFile { filename: filename.into(), source })
    }

    fn empty() -> Self {
        Self {
            notmuch_revision: None,
            jmap_state: None,
        }
    }
}

pub fn sync(
    stdout: &mut StandardStream,
    info_color_spec: ColorSpec,
    mail_dir: PathBuf,
    args: Args,
    config: Config,
    pull: bool,
) -> Result<(), Error> {
    // Grab lock.
    let lock_file_path = mail_dir.join("mujmap.lock");
    let mut lock = LockFile::open(&lock_file_path).map_err(|source| Error::OpenLockFile {
        path: lock_file_path,
        source
    })?;
    let is_locked = lock.try_lock().map_err(|source| Error::Lock {source})?;
    if !is_locked {
        println!("Lock file owned by another process. Waiting...");
        lock.lock().map_err(|source| Error::Lock {source})?;
    }

    // Load the intermediary state.
    let latest_state_filename = mail_dir.join("mujmap.state.json");
    let latest_state = LatestState::open(&latest_state_filename).unwrap_or_else(|e| {
        warn!("{e}");
        LatestState::empty()
    });

    // Open the local notmuch database.
    let local = Local::open(mail_dir, args.dry_run || !pull).map_err(|source| Error::OpenLocal {source})?;

    // Open the local cache.
    let cache = Cache::open(&local.mail_cur_dir, &config).map_err(|source| Error::OpenCache {source})?;

    // Open the remote session.
    let mut remote = Remote::open(&config).map_err(|source| Error::OpenRemote {source})?;

    // List all remote mailboxes and convert them to notmuch tags.
    let mut mailboxes = remote
        .get_mailboxes(&config.tags)
        .map_err(|source| Error::IndexMailboxes {source})?;
    debug!("Got mailboxes: {:?}", mailboxes);

    // Query local database for all email.
    let local_emails = local.all_emails().map_err(|source| Error::IndexLocalEmails {source})?;

    // Function which performs a full sync, i.e. a sync which considers all remote IDs as updated,
    // and determines destroyed IDs by finding the difference of all remote IDs from all local IDs.
    let full_sync =
        |remote: &mut Remote| -> Result<(jmap::State, HashSet<jmap::Id>, HashSet<jmap::Id>)> {
            let (state, updated_ids) = remote.all_email_ids().map_err(|source| Error::IndexRemoteEmails {source})?;
            // TODO can we optimize these two lines?
            let local_ids: HashSet<jmap::Id> = local_emails.keys().cloned().collect();
            let destroyed_ids = local_ids.difference(&updated_ids).cloned().collect();
            Ok((state, updated_ids, destroyed_ids))
        };

    // Create lists of updated and destroyed `Email` IDs. This is done in one of two ways, depending
    // on if we have a working JMAP `Email` state.
    let (state, updated_ids, destroyed_ids) = latest_state
        .jmap_state.clone()
        .map(|jmap_state| {
            match remote.changed_email_ids(jmap_state) {
                Ok((state, created, mut updated, destroyed)) => {
                    debug!("Remote changes: state={state}, created={created:?}, updated={updated:?}, destroyed={destroyed:?}");
                    // If we have something in the updated set that isn't in the local database,
                    // something must have gone wrong somewhere. Do a full sync instead.
                    if !updated.iter().all(|x| local_emails.contains_key(x)) {
                        warn!(
                            "Server sent an update which references an ID we don't know about, doing a full sync instead");
                        full_sync(&mut remote)
                    } else {
                        updated.extend(created);
                        Ok((state, updated, destroyed))
                    }
                },
                Err(e) => {
                    // `Email/changes` failed, so fall back to `Email/query`.
                    warn!(
                        "Error while attempting to resolve changes, attempting full sync: {e}"
                    );
                    full_sync(&mut remote)
                }
            }
        })
        .unwrap_or_else(|| full_sync(&mut remote))?;

    // Retrieve the updated `Email` objects from the server.
    stdout.set_color(&info_color_spec).map_err(|source| Error::Log {source})?;
    write!(stdout, "Retrieving metadata...").map_err(|source| Error::Log {source})?;
    stdout.reset().map_err(|source| Error::Log {source})?;
    writeln!(stdout, " ({} possibly changed)", updated_ids.len()).map_err(|source| Error::Log {source})?;
    stdout.flush().map_err(|source| Error::Log {source})?;

    let remote_emails = remote
        .get_emails(updated_ids.iter(), &mailboxes, &config.tags)
        .map_err(|source| Error::GetRemoteEmails {source})?;

    // Before merging, download the new files into the cache.
    let mut new_emails: HashMap<jmap::Id, NewEmail> = remote_emails
        .values()
        .filter(|remote_email| match local_emails.get(&remote_email.id) {
            Some(local_email) => local_email.blob_id != remote_email.blob_id,
            None => true,
        })
        .map(|remote_email| {
            (
                remote_email.id.clone(),
                NewEmail {
                    remote_email,
                    cache_path: cache.cache_path(&remote_email.id, &remote_email.blob_id),
                    maildir_path: local.new_maildir_path(&remote_email.id, &remote_email.blob_id),
                },
            )
        })
        .collect();

    let new_emails_missing_from_cache: Vec<&NewEmail> = new_emails
        .values()
        .filter(|x| !x.cache_path.exists() && !local_emails.contains_key(&x.remote_email.id))
        .collect();

    if !new_emails_missing_from_cache.is_empty() {
        stdout.set_color(&info_color_spec).map_err(|source| Error::Log {source})?;
        writeln!(stdout, "Downloading new mail...").map_err(|source| Error::Log {source})?;
        stdout.reset().map_err(|source| Error::Log {source})?;
        stdout.flush().map_err(|source| Error::Log {source})?;

        let pb = ProgressBar::new(new_emails_missing_from_cache.len() as u64);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(config.concurrent_downloads)
            .build()
            .map_err(|source| Error::CreateDownloadThreadPool {source})?;
        let result: Result<Vec<_>, Error> = pool.install(|| {
            new_emails_missing_from_cache
                .into_par_iter()
                .map(|new_email| {
                    let mut retry_count = 0;
                    loop {
                        match download(new_email, &remote, &cache, config.convert_dos_to_unix) {
                            Ok(_) => {
                                pb.inc(1);
                                return Ok(());
                            }
                            Err(e) => {
                                // Try again.
                                retry_count += 1;
                                if config.retries > 0 && retry_count >= config.retries {
                                    return Err(e);
                                }
                                warn!("Download error on try {}, retrying: {}", retry_count, e);
                            }
                        };
                    }
                })
                .collect()
        });
        result?;
        pb.finish_with_message("done");
    }

    // Merge locally.
    //
    // 1. Symlink the cached messages that were previously downloaded into the maildir. We will
    // replace these symlinks with the actual files once the atomic sync is complete.
    //
    // 2. Add new messages to the database by indexing these symlinks. This is also done for
    // existing messages which have new blob IDs.
    //
    // 3. Update the tags of all local messages *except* the ones which had been modified locally
    // since mujmap was last run. Neither JMAP nor notmuch support looking at message history, so if
    // both the local message and the remote message have been flagged as "updated" since the last
    // sync, we prefer to overwrite remote tags with notmuch's tags.
    //
    // 4. Remove messages with destroyed IDs or updated blob IDs.
    //
    // 5. Overwrite the symlinks we made earlier with the actual files from the cache.
    let notmuch_revision = get_notmuch_revision(
        local_emails.is_empty(),
        &local,
        latest_state.notmuch_revision,
        args.dry_run,
    )?;
    let updated_local_emails: HashMap<jmap::Id, local::Email> = local
        .all_emails_since(notmuch_revision)
        .map_err(|source| Error::IndexLocalUpdatedEmails {source})?
        .into_iter()
        // Filter out emails that were destroyed on the server.
        .filter(|(id, _)| !destroyed_ids.contains(id))
        .collect();

    if pull {
        stdout.set_color(&info_color_spec).map_err(|source| Error::Log {source})?;
        write!(stdout, "Applying changes to notmuch database...").map_err(|source| Error::Log {source})?;
        stdout.reset().map_err(|source| Error::Log {source})?;
        writeln!(
            stdout,
            " ({} new, {} changed, {} destroyed)",
            new_emails.len(),
            remote_emails.len(),
            destroyed_ids.len()
        )
        .map_err(|source| Error::Log {source})?;
        stdout.flush().map_err(|source| Error::Log {source})?;

        // Update local messages.
        if !args.dry_run {
            // Collect the local messages which will be destroyed. We will add to this list any
            // messages with new blob IDs.
            let mut destroyed_local_emails: Vec<&local::Email> = destroyed_ids
                .into_iter()
                .flat_map(|x| local_emails.get(&x))
                .collect();

            // Symlink the new mail files into the maildir...
            for new_email in new_emails.values() {
                debug!(
                    "Making symlink from `{}' to `{}'",
                    &new_email.cache_path.to_string_lossy(),
                    &new_email.maildir_path.to_string_lossy(),
                );
                if new_email.maildir_path.exists() {
                    warn!(
                        "File `{}' already existed in maildir but was not indexed. Replacing...",
                        &new_email.maildir_path.to_string_lossy(),
                    );
                    fs::remove_file(&new_email.maildir_path).map_err(|source| Error::
                        RemoveUnindexedMailFile {
                            path: new_email.maildir_path.clone(),
                            source
                        },
                    )?;
                }
                symlink_file(&new_email.cache_path, &new_email.maildir_path).map_err(|source| Error::
                    MakeMaildirSymlink {
                        from: new_email.cache_path.clone(),
                        to: new_email.maildir_path.clone(),
                        source
                    },
                )?;
            }

            let mut commit_changes = || -> Result<()> {
                local.begin_atomic().map_err(|source| Error::BeginAtomic {source})?;

                // ...and add them to the database.
                let new_local_emails = new_emails
                    .values()
                    .map(|new_email| {
                        let local_email =
                            local.add_new_email(new_email).map_err(|source| Error::AddLocalEmail {
                                filename: new_email.cache_path.clone(),
                                source
                            })?;
                        if let Some(e) = local_emails.get(&new_email.remote_email.id) {
                            // Move the old message to the destroyed emails set.
                            destroyed_local_emails.push(e);
                        }
                        Ok((local_email.id.clone(), local_email))
                    })
                    .collect::<Result<HashMap<_, _>>>()?;

                // Update local emails with remote tags.
                //
                // XXX: If the server contains two or more of a message which notmuch considers a
                // duplicate, it will be updated *for each duplicate* in a non-deterministic order.
                // This may cause surprises.
                for remote_email in remote_emails.values() {
                    // Skip email which has been updated offline.
                    if updated_local_emails.contains_key(&remote_email.id) {
                        continue;
                    }

                    // Do it!
                    let local_email = [
                        new_local_emails.get(&remote_email.id),
                        local_emails.get(&remote_email.id),
                    ]
                    .into_iter()
                    .flatten()
                    .next()
                    .ok_or_else(|| {
                        error!(
                            "Could not find local email for updated remote ID {}",
                            remote_email.id
                        );
                        Error::Programmer {}
                    })?;

                    // Add mailbox tags
                    let mut tags: HashSet<&str> =
                        remote_email.tags.iter().map(|s| s.as_str()).collect();
                    for id in &remote_email.mailbox_ids {
                        if let Some(mailbox) = mailboxes.mailboxes_by_id.get(id) {
                            tags.insert(&mailbox.tag);
                        }
                    }

                    local
                        .update_email_tags(local_email, tags)
                        .map_err(|source| Error::UpdateLocalEmail {source})?;

                    // In `update' notmuch may have renamed the file on disk when setting maildir
                    // flags, so we need to update our idea of the filename to match so that, for
                    // new messages, we can reliably replace the symlink later.
                    //
                    // The `Message' might have multiple paths though (if more than one message has
                    // the same id) so we have to get all the filenames and then find the one that
                    // matches ours. Fortunately, our generated name (the raw JMAP mailbox.message
                    // id) will always be a substring of notmuch's version (same name with flags
                    // attached), so a starts-with test is enough.
                    if let Some(new_email) = new_emails.get_mut(&remote_email.id) {
                        if let Some(our_filename) = new_email
                            .maildir_path
                            .file_name()
                            .map(|p| p.to_string_lossy())
                        {
                            if let Some(message) = local
                                .get_message(&local_email.message_id)
                                .map_err(|source| Error::GetNotmuchMessage {source})?
                            {
                                if let Some(new_maildir_path) = message.filenames().find(|f| {
                                    f.file_name().is_some_and(|p| {
                                        p.to_string_lossy().starts_with(&*our_filename)
                                    })
                                }) {
                                    new_email.maildir_path = new_maildir_path;
                                }
                            }
                        }
                    }
                }

                // Finally, remove the old messages from the database.
                for destroyed_local_email in &destroyed_local_emails {
                    local
                        .remove_email(destroyed_local_email)
                        .map_err(|source| Error::RemoveLocalEmail {source})?;
                }

                local.end_atomic().map_err(|source| Error::EndAtomic {source})?;
                Ok(())
            };

            if let Err(e) = commit_changes() {
                // Remove all the symlinks.
                for new_email in new_emails.values() {
                    debug!(
                        "Removing symlink `{}'",
                        &new_email.maildir_path.to_string_lossy(),
                    );
                    if let Err(e) = fs::remove_file(&new_email.maildir_path) {
                        warn!(
                            "Could not remove symlink `{}': {e}",
                            &new_email.maildir_path.to_string_lossy(),
                        );
                    }
                }
                // Fail as normal.
                return Err(e);
            }

            // Now that the atomic database operation has been completed, do the actual file
            // operations.

            // Replace the symlinks with the real files.
            for new_email in new_emails.values() {
                debug!(
                    "Moving mail from `{}' to `{}'",
                    &new_email.cache_path.to_string_lossy(),
                    &new_email.maildir_path.to_string_lossy(),
                );
                fs::rename(&new_email.cache_path, &new_email.maildir_path).map_err(|source| Error::
                    RenameMailFile {
                        from: new_email.cache_path.clone(),
                        to: new_email.maildir_path.clone(),
                        source
                    },
                )?;
            }

            // Delete the destroyed email files.
            for destroyed_local_email in &destroyed_local_emails {
                fs::remove_file(&destroyed_local_email.path).map_err(|source| Error::RemoveMailFile {
                    path: destroyed_local_email.path.clone(),
                    source
                })?;
            }
        }
    }

    if !args.dry_run {
        // Ensure that for every tag, there exists a corresponding mailbox.
        let tags_with_missing_mailboxes: Vec<String> = local
            .all_tags()
            .map_err(|source| Error::IndexTags {source})?
            .filter(|tag| {
                let tag = tag.as_str();
                // Any tags which *can* be mapped to a keyword do not require a mailbox.
                // Additionally, automatic tags are never mapped to mailboxes.
                if [
                    "draft",
                    "flagged",
                    "passed",
                    "replied",
                    "unread",
                    &config.tags.spam,
                    &config.tags.important,
                    &config.tags.phishing,
                ]
                .contains(&tag)
                    || local::AUTOMATIC_TAGS.contains(tag)
                {
                    false
                } else {
                    !mailboxes.ids_by_tag.contains_key(tag)
                }
            })
            .collect();
        if !tags_with_missing_mailboxes.is_empty() {
            if !config.auto_create_new_mailboxes {
                return Err(Error::MissingMailboxes {
                    tags: tags_with_missing_mailboxes,
                });
            }
            remote
                .create_mailboxes(&mut mailboxes, &tags_with_missing_mailboxes, &config.tags)
                .map_err(|source| Error::CreateMailboxes {
                    tags: tags_with_missing_mailboxes,
                    source
                })?;
        }
    }

    // Update remote messages.
    stdout.set_color(&info_color_spec).map_err(|source| Error::Log {source})?;
    write!(stdout, "Applying changes to JMAP server...").map_err(|source| Error::Log {source})?;
    stdout.reset().map_err(|source| Error::Log {source})?;
    writeln!(stdout, " ({} changed)", updated_local_emails.len()).map_err(|source| Error::Log {source})?;
    stdout.flush().map_err(|source| Error::Log {source})?;

    if !args.dry_run {
        remote
            .update(&updated_local_emails, &mailboxes, &config.tags)
            .map_err(|source| Error::PushChanges {source})?;
    }

    if !args.dry_run {
        // Record the final state for the next invocation.
        LatestState {
            notmuch_revision: Some(local.revision() + 1),
            jmap_state: if pull {
                Some(state)
            } else {
                latest_state.jmap_state
            },
        }
        .save(latest_state_filename)?;
    }

    Ok(())
}

fn download(
    new_email: &NewEmail,
    remote: &Remote,
    cache: &Cache,
    convert_dos_to_unix: bool,
) -> Result<()> {
    let remote_email = new_email.remote_email;
    let reader = remote
        .read_email_blob(&remote_email.blob_id)
        .map_err(|source| Error::DownloadRemoteEmail {source})?;
    cache
        .download_into_cache(new_email, reader, convert_dos_to_unix)
        .map_err(|source| Error::CacheNewEmail {source})?;
    Ok(())
}

fn get_notmuch_revision(
    has_no_local_emails: bool,
    local: &Local,
    notmuch_revision: Option<u64>,
    dry_run: bool,
) -> Result<u64> {
    match notmuch_revision {
        Some(x) => Ok(x),
        None => {
            if has_no_local_emails {
                Ok(local.revision())
            } else {
                if dry_run {
                    println!(
                        "\
THIS IS A DRY RUN, SO NO CHANGES WILL BE MADE NO MATTER THE CHOICE. HOWEVER,
HEED THE WARNING FOR THE REAL DEAL.
"
                    );
                }
                println!(
                    "\
mujmap was unable to read the notmuch database revision (stored in
mujmap.state.json) since the last time it was run. As a result, it cannot
determine the changes made in the local database since the last time a
synchronization was performed.
"
                );
                if atty::is(Stream::Stdout) {
                    println!(
                        "\
If you continue, any potential changes made to your database since your last
sync will not be pushed to the JMAP server, and some may be overwritten by the
JMAP server's state. Depending on your situation, this has potential to create
inconsistencies between the state of your database and the state of the server.

If this is the first time you are synchronizing mujmap in a pre-existing
database, this is not an issue. If the mail files in your database are different
from your mail on the JMAP server, you can proceed and mujmap will perform the
initial setup with no issues. This would be the case if you had multiple email
accounts in the same database, for example.

If this is the first time you are synchronizing, and you are attempting to
migrate your existing notmuch database tags to mailboxes on a JMAP server,
DO NOT continue. Instead, follow the relevant instructions in mujmap's README.

If this is the first time you are synchronizing, but you do not care about your
notmuch tags and would like them to be replaced with the JMAP server's state,
you may continue.

If this is NOT the first time you are synchronizing, you should quit and force
a full sync by deleting the `mujmap.state.json' file and invoking mujmap again.
This will overwrite all of your local JMAP state with the JMAP server's state.
You will encounter this message again, but at that point you can safely proceed.

Continue? (y/N)
"
                    );
                    let mut response = String::new();
                    io::stdin().read_line(&mut response).ok();
                    let trimmed = response.trim();
                    if !(trimmed == "y" || trimmed == "Y") {
                        Err(Error::MissingNotmuchDatabaseRevision {})?
                    }
                    Ok(local.revision())
                } else {
                    println!(
                        "\
Please run mujmap again in an interactive terminal to resolve.
"
                    );
                    Err(Error::MissingNotmuchDatabaseRevision {})
                }
            }
        }
    }
}
