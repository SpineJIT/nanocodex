//! Durable SQLite worksets without execution policy.
//!
//! A workset records the complete desired coordinate matrix, but never chooses a
//! task for its caller. Callers select an exact coordinate family and the
//! ledger atomically allocates one fungible repetition within that family.

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OptionalExtension as _, Transaction, TransactionBehavior, params};
use serde::Serialize;
use uuid::Uuid;

const SCHEMA_VERSION: u32 = 3;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// One task package included in a workset.
#[derive(Clone, Debug)]
pub struct WorksetTask {
    /// Profile-visible task selector.
    pub selector: String,
    /// Loaded task name.
    pub name: String,
    /// Canonical task root.
    pub root: PathBuf,
    /// Task package content digest.
    pub digest: String,
}

/// One exact treatment with a ledger-owned repetition count.
#[derive(Clone, Debug)]
pub struct WorksetFamily {
    /// Stable identity of all semantic knobs except repetition.
    pub key: String,
    /// Task selector referenced by this family.
    pub task_selector: String,
    /// Stable serialized treatment description.
    pub treatment: String,
    /// Number of fungible desired repetitions.
    pub trials: u16,
}

/// Complete work definition read back from SQLite.
#[derive(Clone, Debug)]
pub struct WorksetDefinition {
    /// User-visible name of the durable board.
    pub profile: String,
    /// Stable identifier of this board generation.
    pub digest: String,
    /// Task packages retained by the board.
    pub tasks: Vec<WorksetTask>,
    /// Exact treatments and desired repetition counts retained by the board.
    pub families: Vec<WorksetFamily>,
}

/// Durable SQLite workset ledger.
#[derive(Clone, Debug)]
pub struct Workset {
    path: PathBuf,
    id: i64,
    profile: String,
    digest: String,
}

/// Result of requesting one repetition from an exact coordinate family.
#[derive(Clone, Debug)]
pub enum BeginCoordinate {
    /// This task's shared preparation must be performed first.
    Prepare(PreparationLease),
    /// One repetition was claimed for execution.
    Execute(CoordinateLease),
    /// Matching work exists but is currently owned by another process.
    Busy(WorksetBusy),
    /// Every desired repetition in the family is terminal.
    Complete,
}

/// Fenced ownership of one task preparation.
#[derive(Clone, Debug)]
pub struct PreparationLease {
    task_id: i64,
    generation: i64,
    owner: String,
}

/// Fenced ownership of one internal workset repetition.
#[derive(Clone, Debug)]
pub struct CoordinateLease {
    coordinate_id: i64,
    execution_id: i64,
    pub(crate) generation: i64,
    owner: String,
    /// Internal fungible repetition allocated by SQLite.
    pub repetition: u16,
}

/// Temporary inability to progress an explicitly selected family.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorksetBusy {
    /// Stable machine-readable reason.
    pub reason: &'static str,
    /// Suggested delay before another inspection or run request.
    pub retry_after_ms: u64,
}

/// Workset-level durable status.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorksetStatus {
    /// Profile name.
    pub profile: String,
    /// Immutable resolved-profile digest.
    pub digest: String,
    /// Task-preparation counts.
    pub preparation: StateCounts,
    /// Coordinate execution counts.
    pub coordinates: CoordinateCounts,
    /// Exact family-level status records.
    pub families: Vec<FamilyStatus>,
}

/// Counts for a small durable state machine.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct StateCounts {
    /// Work not currently owned.
    pub pending: i64,
    /// Work with an active lease.
    pub running: i64,
    /// Successfully terminal work.
    pub complete: i64,
}

/// Coordinate counts including retry diagnostics.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct CoordinateCounts {
    /// Repetitions available to run or retry.
    pub pending: i64,
    /// Repetitions with active leases.
    pub running: i64,
    /// Accepted terminal repetitions.
    pub terminal: i64,
}

/// Status of one exact task/treatment family.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FamilyStatus {
    /// Stable family identity.
    pub key: String,
    /// Profile-visible task selector.
    pub task: String,
    /// Network identity of the host that owns preparation and execution.
    pub assigned_host: Option<String>,
    /// Stable serialized treatment description.
    pub treatment: String,
    /// Desired repetition count.
    pub desired: i64,
    /// Available repetition count.
    pub pending: i64,
    /// Actively leased repetition count.
    pub running: i64,
    /// Accepted terminal repetition count.
    pub terminal: i64,
}

/// Durable ledger failure.
#[derive(Debug, thiserror::Error)]
pub enum WorksetError {
    /// Ledger parent directory could not be created.
    #[error("failed to create durable workset directory: {0}")]
    Io(#[from] std::io::Error),
    /// SQLite operation failed.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    /// No generation exists for the requested workset name.
    #[error("evaluation workset `{0}` does not exist")]
    UnknownWorkset(String),
    /// The selected family is not part of the initialized workset.
    #[error("coordinate family `{0}` is not part of this workset")]
    UnknownFamily(String),
    /// A family references a task absent from the same workset.
    #[error("coordinate family `{family}` references unknown task `{task}`")]
    UnknownTask {
        /// Family containing the invalid reference.
        family: String,
        /// Missing profile-visible task selector.
        task: String,
    },
    /// Appended work disagrees with a definition already retained in SQLite.
    #[error("workset `{0}` conflicts with its retained SQLite definition")]
    DefinitionConflict(String),
    /// A stale worker attempted to mutate work after losing its lease.
    #[error("stale {kind} lease was fenced before it could commit")]
    StaleLease {
        /// Kind of fenced work.
        kind: &'static str,
    },
    /// A numeric value could not be represented by the durable schema.
    #[error("durable workset value is out of range: {0}")]
    OutOfRange(&'static str),
}

impl Workset {
    /// Opens the newest generation of a named durable workset.
    pub fn open(path: impl Into<PathBuf>, profile: &str) -> Result<Self, WorksetError> {
        let path = path.into();
        let mut connection = open_connection(&path)?;
        initialize_schema(&mut connection)?;
        let retained: Option<(i64, String)> = connection
            .query_row(
                "SELECT id, digest FROM worksets WHERE profile = ?1 \
                 ORDER BY created_at_ms DESC, id DESC LIMIT 1",
                [profile],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((id, digest)) = retained else {
            return Err(WorksetError::UnknownWorkset(profile.to_owned()));
        };
        Ok(Self {
            path,
            id,
            profile: profile.to_owned(),
            digest,
        })
    }

    /// Creates a new empty generation of a named workset.
    pub fn create(path: impl Into<PathBuf>, profile: &str) -> Result<Self, WorksetError> {
        let path = path.into();
        let mut connection = open_connection(&path)?;
        initialize_schema(&mut connection)?;
        let digest = Uuid::now_v7().simple().to_string();
        connection.execute(
            "INSERT INTO worksets(profile, digest, config_path, created_at_ms) \
             VALUES (?1, ?2, '', ?3)",
            params![profile, digest, now_ms()?],
        )?;
        Ok(Self {
            path,
            id: connection.last_insert_rowid(),
            profile: profile.to_owned(),
            digest,
        })
    }

    /// Idempotently appends tasks, treatments, and any missing repetitions.
    /// Existing accepted work is never rewritten or removed.
    pub fn append(
        &self,
        tasks: &[WorksetTask],
        families: &[WorksetFamily],
    ) -> Result<(), WorksetError> {
        let mut connection = open_connection(&self.path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        append_definition(&transaction, self.id, &self.digest, tasks, families)?;
        transaction.commit()?;
        Ok(())
    }

    /// Reads the concrete task and coordinate definitions retained in SQLite.
    pub fn definition(&self) -> Result<WorksetDefinition, WorksetError> {
        let connection = open_connection(&self.path)?;
        let mut tasks = connection.prepare(
            "SELECT selector, name, root, digest FROM tasks \
             WHERE workset_id = ?1 ORDER BY selector",
        )?;
        let tasks = tasks
            .query_map([self.id], |row| {
                Ok(WorksetTask {
                    selector: row.get(0)?,
                    name: row.get(1)?,
                    root: PathBuf::from(row.get::<_, String>(2)?),
                    digest: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut families = connection.prepare(
            "SELECT c.family_key, t.selector, c.treatment, COUNT(*) \
             FROM coordinates c JOIN tasks t ON t.id = c.task_id \
             WHERE c.workset_id = ?1 \
             GROUP BY c.family_key, t.selector, c.treatment \
             ORDER BY t.selector, c.family_key",
        )?;
        let families = families
            .query_map([self.id], |row| {
                let trials: i64 = row.get(3)?;
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, trials))
            })?
            .map(|row| {
                let (key, task_selector, treatment, trials) = row?;
                Ok(WorksetFamily {
                    key,
                    task_selector,
                    treatment,
                    trials: u16::try_from(trials)
                        .map_err(|_| WorksetError::OutOfRange("family trials"))?,
                })
            })
            .collect::<Result<Vec<_>, WorksetError>>()?;
        Ok(WorksetDefinition {
            profile: self.profile.clone(),
            digest: self.digest.clone(),
            tasks,
            families,
        })
    }

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Atomically begins preparation or one available repetition from the
    /// exact caller-selected family.
    #[cfg(test)]
    fn begin(
        &self,
        family_key: &str,
        lease_duration: Duration,
    ) -> Result<BeginCoordinate, WorksetError> {
        self.begin_for_host(family_key, "local", lease_duration)
    }

    /// Atomically begins work for one family on its assigned host.
    pub fn begin_for_host(
        &self,
        family_key: &str,
        host: &str,
        lease_duration: Duration,
    ) -> Result<BeginCoordinate, WorksetError> {
        let owner = Uuid::now_v7().to_string();
        let now = now_ms()?;
        let expires = lease_expiry(now, lease_duration)?;
        let mut connection = open_connection(&self.path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task_id: Option<i64> = transaction
            .query_row(
                "SELECT c.task_id FROM coordinates c \
                 WHERE c.workset_id = ?1 AND c.family_key = ?2 LIMIT 1",
                params![self.id, family_key],
                |row| row.get(0),
            )
            .optional()?;
        let Some(task_id) = task_id else {
            return Err(WorksetError::UnknownFamily(family_key.to_owned()));
        };
        let preparation: (String, i64, Option<i64>, Option<String>) = transaction.query_row(
            "SELECT preparation_state, preparation_generation, preparation_expires_at_ms, \
                    assigned_host \
             FROM tasks WHERE id = ?1",
            [task_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        match preparation.3.as_deref() {
            None => {
                transaction.execute(
                    "UPDATE tasks SET assigned_host = ?1 WHERE id = ?2 AND assigned_host IS NULL",
                    params![host, task_id],
                )?;
            }
            Some(assigned) if assigned == host => {}
            Some(_) => {
                transaction.commit()?;
                return Ok(BeginCoordinate::Busy(WorksetBusy {
                    reason: "task_assigned_elsewhere",
                    retry_after_ms: 30_000,
                }));
            }
        }
        if preparation.0 != "ready" {
            let reclaimable = preparation.0 == "pending"
                || (preparation.0 == "preparing"
                    && preparation.2.is_some_and(|deadline| deadline <= now));
            if !reclaimable {
                transaction.commit()?;
                return Ok(BeginCoordinate::Busy(WorksetBusy {
                    reason: "task_preparing",
                    retry_after_ms: retry_after(preparation.2, now),
                }));
            }
            let generation = preparation
                .1
                .checked_add(1)
                .ok_or(WorksetError::OutOfRange("preparation generation"))?;
            transaction.execute(
                "UPDATE tasks SET preparation_state = 'preparing', preparation_generation = ?1, \
                    preparation_owner = ?2, preparation_expires_at_ms = ?3, preparation_error = NULL \
                 WHERE id = ?4",
                params![generation, owner, expires, task_id],
            )?;
            transaction.commit()?;
            return Ok(BeginCoordinate::Prepare(PreparationLease {
                task_id,
                generation,
                owner,
            }));
        }

        let coordinate: Option<(i64, i64, u16)> = transaction
            .query_row(
                "SELECT id, generation, repetition FROM coordinates \
                 WHERE workset_id = ?1 AND family_key = ?2 \
                   AND (state = 'pending' OR (state = 'running' AND lease_expires_at_ms <= ?3)) \
                 ORDER BY repetition LIMIT 1",
                params![self.id, family_key, now],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((coordinate_id, previous_generation, repetition)) = coordinate {
            let generation = previous_generation
                .checked_add(1)
                .ok_or(WorksetError::OutOfRange("coordinate generation"))?;
            transaction.execute(
                "UPDATE executions SET state = 'expired', finished_at_ms = ?1, \
                    error = 'lease expired and was reclaimed' \
                 WHERE coordinate_id = ?2 AND generation = ?3 AND state = 'running'",
                params![now, coordinate_id, previous_generation],
            )?;
            transaction.execute(
                "UPDATE coordinates SET state = 'running', generation = ?1, lease_owner = ?2, \
                    lease_expires_at_ms = ?3, last_error = NULL WHERE id = ?4",
                params![generation, owner, expires, coordinate_id],
            )?;
            transaction.execute(
                "INSERT INTO executions(coordinate_id, generation, owner, started_at_ms, state) \
                 VALUES (?1, ?2, ?3, ?4, 'running')",
                params![coordinate_id, generation, owner, now],
            )?;
            let execution_id = transaction.last_insert_rowid();
            transaction.commit()?;
            return Ok(BeginCoordinate::Execute(CoordinateLease {
                coordinate_id,
                execution_id,
                generation,
                owner,
                repetition,
            }));
        }
        let (running, terminal): (i64, i64) = transaction.query_row(
            "SELECT \
                COALESCE(SUM(state = 'running'), 0), \
                COALESCE(SUM(state = 'terminal'), 0) \
             FROM coordinates WHERE workset_id = ?1 AND family_key = ?2",
            params![self.id, family_key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let total: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM coordinates WHERE workset_id = ?1 AND family_key = ?2",
            params![self.id, family_key],
            |row| row.get(0),
        )?;
        transaction.commit()?;
        if terminal == total {
            Ok(BeginCoordinate::Complete)
        } else {
            debug_assert!(running > 0);
            Ok(BeginCoordinate::Busy(WorksetBusy {
                reason: "coordinates_running",
                retry_after_ms: 1_000,
            }))
        }
    }

    /// Extends a preparation lease while work remains live.
    pub fn heartbeat_preparation(
        &self,
        lease: &PreparationLease,
        lease_duration: Duration,
    ) -> Result<(), WorksetError> {
        self.update_preparation_lease(lease, lease_duration, None)
    }

    /// Fenced completion of shared task preparation.
    pub fn complete_preparation(&self, lease: &PreparationLease) -> Result<(), WorksetError> {
        let connection = open_connection(&self.path)?;
        let changed = connection.execute(
            "UPDATE tasks SET preparation_state = 'ready', preparation_owner = NULL, \
                preparation_expires_at_ms = NULL, preparation_error = NULL \
             WHERE id = ?1 AND preparation_state = 'preparing' \
               AND preparation_generation = ?2 AND preparation_owner = ?3",
            params![lease.task_id, lease.generation, lease.owner],
        )?;
        fenced(changed, "preparation")
    }

    /// Makes a failed preparation retryable while retaining its diagnostic.
    pub fn retry_preparation(
        &self,
        lease: &PreparationLease,
        error: &str,
    ) -> Result<(), WorksetError> {
        let connection = open_connection(&self.path)?;
        let changed = connection.execute(
            "UPDATE tasks SET preparation_state = 'pending', preparation_owner = NULL, \
                preparation_expires_at_ms = NULL, preparation_error = ?1 \
             WHERE id = ?2 AND preparation_state = 'preparing' \
               AND preparation_generation = ?3 AND preparation_owner = ?4",
            params![error, lease.task_id, lease.generation, lease.owner],
        )?;
        fenced(changed, "preparation")
    }

    /// Extends a coordinate lease while its process remains live.
    pub fn heartbeat_coordinate(
        &self,
        lease: &CoordinateLease,
        lease_duration: Duration,
    ) -> Result<(), WorksetError> {
        let now = now_ms()?;
        let expires = lease_expiry(now, lease_duration)?;
        let connection = open_connection(&self.path)?;
        let changed = connection.execute(
            "UPDATE coordinates SET lease_expires_at_ms = ?1 \
             WHERE id = ?2 AND state = 'running' AND generation = ?3 AND lease_owner = ?4",
            params![expires, lease.coordinate_id, lease.generation, lease.owner],
        )?;
        fenced(changed, "coordinate")
    }

    /// Fenced acceptance of one terminal result.
    pub fn complete_coordinate(
        &self,
        lease: &CoordinateLease,
        result_path: &Path,
    ) -> Result<(), WorksetError> {
        self.finish_coordinate(lease, "terminal", Some(result_path), None)
    }

    /// Makes an execution failure retryable without allocating another trial.
    pub fn retry_coordinate(
        &self,
        lease: &CoordinateLease,
        error: &str,
    ) -> Result<(), WorksetError> {
        self.finish_coordinate(lease, "pending", None, Some(error))
    }

    /// Reads a complete workset and family status snapshot.
    pub fn status(&self) -> Result<WorksetStatus, WorksetError> {
        let connection = open_connection(&self.path)?;
        let now = now_ms()?;
        let preparation = connection.query_row(
            "SELECT \
                COALESCE(SUM(preparation_state = 'pending' OR \
                    (preparation_state = 'preparing' AND preparation_expires_at_ms <= ?2)), 0), \
                COALESCE(SUM(preparation_state = 'preparing' AND \
                    preparation_expires_at_ms > ?2), 0), \
                COALESCE(SUM(preparation_state = 'ready'), 0) \
             FROM tasks WHERE workset_id = ?1",
            params![self.id, now],
            |row| {
                Ok(StateCounts {
                    pending: row.get(0)?,
                    running: row.get(1)?,
                    complete: row.get(2)?,
                })
            },
        )?;
        let coordinates = coordinate_counts(&connection, self.id, now)?;
        let mut statement = connection.prepare(
            "SELECT c.family_key, t.selector, t.assigned_host, c.treatment, COUNT(*), \
                COALESCE(SUM(c.state = 'pending' OR \
                    (c.state = 'running' AND c.lease_expires_at_ms <= ?2)), 0), \
                COALESCE(SUM(c.state = 'running' AND c.lease_expires_at_ms > ?2), 0), \
                COALESCE(SUM(c.state = 'terminal'), 0) \
             FROM coordinates c JOIN tasks t ON t.id = c.task_id \
             WHERE c.workset_id = ?1 \
             GROUP BY c.family_key, t.selector, t.assigned_host, c.treatment \
             ORDER BY t.selector, c.family_key",
        )?;
        let families = statement
            .query_map(params![self.id, now], |row| {
                Ok(FamilyStatus {
                    key: row.get(0)?,
                    task: row.get(1)?,
                    assigned_host: row.get(2)?,
                    treatment: row.get(3)?,
                    desired: row.get(4)?,
                    pending: row.get(5)?,
                    running: row.get(6)?,
                    terminal: row.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(WorksetStatus {
            profile: self.profile.clone(),
            digest: self.digest.clone(),
            preparation,
            coordinates,
            families,
        })
    }

    fn update_preparation_lease(
        &self,
        lease: &PreparationLease,
        lease_duration: Duration,
        error: Option<&str>,
    ) -> Result<(), WorksetError> {
        let now = now_ms()?;
        let expires = lease_expiry(now, lease_duration)?;
        let connection = open_connection(&self.path)?;
        let changed = connection.execute(
            "UPDATE tasks SET preparation_expires_at_ms = ?1, preparation_error = ?2 \
             WHERE id = ?3 AND preparation_state = 'preparing' \
               AND preparation_generation = ?4 AND preparation_owner = ?5",
            params![expires, error, lease.task_id, lease.generation, lease.owner],
        )?;
        fenced(changed, "preparation")
    }

    fn finish_coordinate(
        &self,
        lease: &CoordinateLease,
        next_state: &str,
        result_path: Option<&Path>,
        error: Option<&str>,
    ) -> Result<(), WorksetError> {
        let mut connection = open_connection(&self.path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE coordinates SET state = ?1, lease_owner = NULL, lease_expires_at_ms = NULL, \
                result_path = ?2, last_error = ?3 \
             WHERE id = ?4 AND state = 'running' AND generation = ?5 AND lease_owner = ?6",
            params![
                next_state,
                result_path.map(|path| path.to_string_lossy()),
                error,
                lease.coordinate_id,
                lease.generation,
                lease.owner
            ],
        )?;
        fenced(changed, "coordinate")?;
        transaction.execute(
            "UPDATE executions SET state = ?1, finished_at_ms = ?2, result_path = ?3, error = ?4 \
             WHERE id = ?5 AND generation = ?6 AND owner = ?7 AND state = 'running'",
            params![
                if next_state == "terminal" {
                    "terminal"
                } else {
                    "retryable"
                },
                now_ms()?,
                result_path.map(|path| path.to_string_lossy()),
                error,
                lease.execution_id,
                lease.generation,
                lease.owner
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }
}

fn append_definition(
    transaction: &Transaction<'_>,
    workset_id: i64,
    workset_digest: &str,
    tasks: &[WorksetTask],
    families: &[WorksetFamily],
) -> Result<(), WorksetError> {
    for task in tasks {
        transaction.execute(
            "INSERT OR IGNORE INTO tasks( \
                workset_id, selector, name, root, digest, preparation_state, preparation_generation \
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0)",
            params![
                workset_id,
                task.selector,
                task.name,
                task.root.to_string_lossy(),
                task.digest
            ],
        )?;
        let retained: Option<(String, String, String)> = transaction
            .query_row(
                "SELECT name, root, digest FROM tasks WHERE workset_id = ?1 AND selector = ?2",
                params![workset_id, task.selector],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if retained.as_ref()
            != Some(&(
                task.name.clone(),
                task.root.to_string_lossy().into_owned(),
                task.digest.clone(),
            ))
        {
            return Err(WorksetError::DefinitionConflict(workset_digest.to_owned()));
        }
    }
    for family in families {
        let task_id: Option<i64> = transaction
            .query_row(
                "SELECT id FROM tasks WHERE workset_id = ?1 AND selector = ?2",
                params![workset_id, family.task_selector],
                |row| row.get(0),
            )
            .optional()?;
        let Some(task_id) = task_id else {
            return Err(WorksetError::UnknownTask {
                family: family.key.clone(),
                task: family.task_selector.clone(),
            });
        };
        let retained: Option<(i64, String)> = transaction
            .query_row(
                "SELECT task_id, treatment FROM coordinates \
                 WHERE workset_id = ?1 AND family_key = ?2 LIMIT 1",
                params![workset_id, family.key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if retained
            .as_ref()
            .is_some_and(|retained| retained != &(task_id, family.treatment.clone()))
        {
            return Err(WorksetError::DefinitionConflict(workset_digest.to_owned()));
        }
        for repetition in 1..=family.trials {
            transaction.execute(
                "INSERT OR IGNORE INTO coordinates( \
                    workset_id, task_id, family_key, treatment, repetition, state, generation \
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0)",
                params![
                    workset_id,
                    task_id,
                    family.key,
                    family.treatment,
                    repetition
                ],
            )?;
        }
    }
    Ok(())
}

fn open_connection(path: &Path) -> Result<Connection, WorksetError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let connection = Connection::open(path)?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    Ok(connection)
}

fn initialize_schema(connection: &mut Connection) -> Result<(), WorksetError> {
    let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(WorksetError::DefinitionConflict(format!(
            "schema {version}; expected {SCHEMA_VERSION}"
        )));
    }
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS worksets(
            id INTEGER PRIMARY KEY,
            profile TEXT NOT NULL,
            digest TEXT NOT NULL,
            config_path TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL,
            UNIQUE(profile, digest)
         );
         CREATE TABLE IF NOT EXISTS tasks(
            id INTEGER PRIMARY KEY,
            workset_id INTEGER NOT NULL REFERENCES worksets(id),
            selector TEXT NOT NULL,
            name TEXT NOT NULL,
            root TEXT NOT NULL,
            digest TEXT NOT NULL,
            preparation_state TEXT NOT NULL CHECK(preparation_state IN ('pending','preparing','ready')),
            preparation_generation INTEGER NOT NULL,
            preparation_owner TEXT,
            preparation_expires_at_ms INTEGER,
            preparation_error TEXT,
            assigned_host TEXT,
            UNIQUE(workset_id, selector)
         );
         CREATE TABLE IF NOT EXISTS coordinates(
            id INTEGER PRIMARY KEY,
            workset_id INTEGER NOT NULL REFERENCES worksets(id),
            task_id INTEGER NOT NULL REFERENCES tasks(id),
            family_key TEXT NOT NULL,
            treatment TEXT NOT NULL,
            repetition INTEGER NOT NULL,
            state TEXT NOT NULL CHECK(state IN ('pending','running','terminal')),
            generation INTEGER NOT NULL,
            lease_owner TEXT,
            lease_expires_at_ms INTEGER,
            result_path TEXT,
            last_error TEXT,
            UNIQUE(workset_id, family_key, repetition)
         );
         CREATE TABLE IF NOT EXISTS executions(
            id INTEGER PRIMARY KEY,
            coordinate_id INTEGER NOT NULL REFERENCES coordinates(id),
            generation INTEGER NOT NULL,
            owner TEXT NOT NULL,
            started_at_ms INTEGER NOT NULL,
            finished_at_ms INTEGER,
            state TEXT NOT NULL CHECK(state IN ('running','terminal','retryable','expired')),
            result_path TEXT,
            error TEXT,
            UNIQUE(coordinate_id, generation)
         );",
    )?;
    if version == 1 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute("ALTER TABLE tasks ADD COLUMN assigned_host TEXT", [])?;
        transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        transaction.commit()?;
    } else {
        connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    Ok(())
}

fn coordinate_counts(
    connection: &Connection,
    workset_id: i64,
    now: i64,
) -> Result<CoordinateCounts, WorksetError> {
    Ok(connection.query_row(
        "SELECT \
            COALESCE(SUM(state = 'pending' OR \
                (state = 'running' AND lease_expires_at_ms <= ?2)), 0), \
            COALESCE(SUM(state = 'running' AND lease_expires_at_ms > ?2), 0), \
            COALESCE(SUM(state = 'terminal'), 0) \
         FROM coordinates WHERE workset_id = ?1",
        params![workset_id, now],
        |row| {
            Ok(CoordinateCounts {
                pending: row.get(0)?,
                running: row.get(1)?,
                terminal: row.get(2)?,
            })
        },
    )?)
}

fn now_ms() -> Result<i64, WorksetError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| WorksetError::OutOfRange("system time"))?
        .as_millis();
    i64::try_from(millis).map_err(|_| WorksetError::OutOfRange("system time"))
}

fn lease_expiry(now: i64, duration: Duration) -> Result<i64, WorksetError> {
    let millis = i64::try_from(duration.as_millis())
        .map_err(|_| WorksetError::OutOfRange("lease duration"))?;
    now.checked_add(millis)
        .ok_or(WorksetError::OutOfRange("lease expiry"))
}

fn retry_after(deadline: Option<i64>, now: i64) -> u64 {
    deadline
        .and_then(|deadline| u64::try_from(deadline.saturating_sub(now)).ok())
        .unwrap_or(1_000)
        .clamp(100, 30_000)
}

const fn fenced(changed: usize, kind: &'static str) -> Result<(), WorksetError> {
    if changed == 1 {
        Ok(())
    } else {
        Err(WorksetError::StaleLease { kind })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition(root: &Path, trials: u16) -> (Vec<WorksetTask>, Vec<WorksetFamily>) {
        (
            vec![WorksetTask {
                selector: "terminal/fix-git".to_owned(),
                name: "fix-git".to_owned(),
                root: root.join("fix-git"),
                digest: "task-digest".to_owned(),
            }],
            vec![WorksetFamily {
                key: "terminal/fix-git|harness|high".to_owned(),
                task_selector: "terminal/fix-git".to_owned(),
                treatment: "codex high".to_owned(),
                trials,
            }],
        )
    }

    fn workset(directory: &Path, trials: u16) -> Workset {
        let workset = Workset::create(directory.join("state.sqlite3"), "release").unwrap();
        let (tasks, families) = definition(directory, trials);
        workset.append(&tasks, &families).unwrap();
        workset
    }

    fn prepared_workset(directory: &Path, trials: u16) -> Workset {
        let workset = workset(directory, trials);
        let BeginCoordinate::Prepare(preparation) = workset
            .begin("terminal/fix-git|harness|high", Duration::from_secs(30))
            .unwrap()
        else {
            panic!("first request must own preparation");
        };
        workset.complete_preparation(&preparation).unwrap();
        workset
    }

    #[test]
    fn append_materializes_every_repetition_before_execution() {
        let directory = tempfile::tempdir().unwrap();
        let workset = workset(directory.path(), 3);
        let status = workset.status().unwrap();

        assert_eq!(status.preparation.pending, 1);
        assert_eq!(status.coordinates.pending, 3);
        assert_eq!(status.families[0].desired, 3);
    }

    #[test]
    fn named_workset_can_be_reopened_and_extended_without_a_manifest() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let workset = Workset::create(&path, "release").unwrap();
        let (tasks, families) = definition(directory.path(), 2);
        workset.append(&tasks, &families).unwrap();
        let reopened = Workset::open(&path, "release").unwrap();
        let retained = reopened.definition().unwrap();

        assert_eq!(retained.profile, "release");
        assert_eq!(retained.digest, workset.digest);
        assert_eq!(retained.tasks.len(), 1);
        assert_eq!(retained.families[0].trials, 2);

        let (tasks, families) = definition(directory.path(), 5);
        reopened.append(&tasks, &families).unwrap();
        assert_eq!(reopened.status().unwrap().coordinates.pending, 5);
        assert_eq!(reopened.definition().unwrap().families[0].trials, 5);
    }

    #[test]
    fn new_generation_does_not_mutate_the_previous_board() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let first = Workset::create(&path, "release").unwrap();
        let (tasks, families) = definition(directory.path(), 1);
        first.append(&tasks, &families).unwrap();
        let second = Workset::create(&path, "release").unwrap();

        assert_ne!(first.digest, second.digest);
        assert_eq!(
            Workset::open(&path, "release").unwrap().digest,
            second.digest
        );
        assert!(second.definition().unwrap().tasks.is_empty());
        assert_eq!(first.definition().unwrap().tasks.len(), 1);
    }

    #[test]
    fn concurrent_callers_allocate_distinct_internal_repetitions() {
        let directory = tempfile::tempdir().unwrap();
        let workset = prepared_workset(directory.path(), 2);
        let BeginCoordinate::Execute(first) = workset
            .begin("terminal/fix-git|harness|high", Duration::from_secs(30))
            .unwrap()
        else {
            panic!("first coordinate should execute");
        };
        let BeginCoordinate::Execute(second) = workset
            .begin("terminal/fix-git|harness|high", Duration::from_secs(30))
            .unwrap()
        else {
            panic!("second coordinate should execute");
        };

        assert_ne!(first.repetition, second.repetition);
        assert!(matches!(
            workset
                .begin("terminal/fix-git|harness|high", Duration::from_secs(30))
                .unwrap(),
            BeginCoordinate::Busy(WorksetBusy {
                reason: "coordinates_running",
                ..
            })
        ));
    }

    #[test]
    fn one_host_owns_task_preparation_and_every_coordinate() {
        let directory = tempfile::tempdir().unwrap();
        let workset = workset(directory.path(), 1);
        let BeginCoordinate::Prepare(preparation) = workset
            .begin_for_host(
                "terminal/fix-git|harness|high",
                "100.64.0.1",
                Duration::from_secs(30),
            )
            .unwrap()
        else {
            panic!("first host should own preparation");
        };
        assert!(matches!(
            workset
                .begin_for_host(
                    "terminal/fix-git|harness|high",
                    "100.64.0.2",
                    Duration::from_secs(30),
                )
                .unwrap(),
            BeginCoordinate::Busy(WorksetBusy {
                reason: "task_assigned_elsewhere",
                ..
            })
        ));
        workset.complete_preparation(&preparation).unwrap();
        let BeginCoordinate::Execute(_) = workset
            .begin_for_host(
                "terminal/fix-git|harness|high",
                "100.64.0.1",
                Duration::from_secs(30),
            )
            .unwrap()
        else {
            panic!("assigned host should execute");
        };
        assert_eq!(
            workset.status().unwrap().families[0]
                .assigned_host
                .as_deref(),
            Some("100.64.0.1")
        );
    }

    #[test]
    fn expired_preparation_is_reported_pending_and_fenced_on_reclamation() {
        let directory = tempfile::tempdir().unwrap();
        let workset = workset(directory.path(), 1);
        let BeginCoordinate::Prepare(stale) = workset
            .begin("terminal/fix-git|harness|high", Duration::ZERO)
            .unwrap()
        else {
            panic!("first request should prepare");
        };
        let status = workset.status().unwrap();
        assert_eq!(status.preparation.pending, 1);
        assert_eq!(status.preparation.running, 0);

        let BeginCoordinate::Prepare(replacement) = workset
            .begin("terminal/fix-git|harness|high", Duration::from_secs(30))
            .unwrap()
        else {
            panic!("expired preparation should be reclaimed");
        };
        assert!(matches!(
            workset.complete_preparation(&stale),
            Err(WorksetError::StaleLease {
                kind: "preparation"
            })
        ));
        workset.complete_preparation(&replacement).unwrap();
    }

    #[test]
    fn expired_worker_is_fenced_after_reclamation() {
        let directory = tempfile::tempdir().unwrap();
        let workset = prepared_workset(directory.path(), 1);
        let BeginCoordinate::Execute(stale) = workset
            .begin("terminal/fix-git|harness|high", Duration::ZERO)
            .unwrap()
        else {
            panic!("coordinate should execute");
        };
        let status = workset.status().unwrap();
        assert_eq!(status.coordinates.pending, 1);
        assert_eq!(status.coordinates.running, 0);
        let BeginCoordinate::Execute(replacement) = workset
            .begin("terminal/fix-git|harness|high", Duration::from_secs(30))
            .unwrap()
        else {
            panic!("expired coordinate should be reclaimed");
        };
        let connection = open_connection(workset.path()).unwrap();
        let expired: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM executions WHERE state = 'expired'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(expired, 1);

        assert!(matches!(
            workset.complete_coordinate(&stale, Path::new("stale")),
            Err(WorksetError::StaleLease { kind: "coordinate" })
        ));
        workset
            .complete_coordinate(&replacement, Path::new("accepted"))
            .unwrap();
        assert_eq!(workset.status().unwrap().coordinates.terminal, 1);
    }

    #[test]
    fn retry_reuses_the_same_profile_trial() {
        let directory = tempfile::tempdir().unwrap();
        let workset = prepared_workset(directory.path(), 1);
        let BeginCoordinate::Execute(first) = workset
            .begin("terminal/fix-git|harness|high", Duration::from_secs(30))
            .unwrap()
        else {
            panic!("coordinate should execute");
        };
        workset.retry_coordinate(&first, "host rebooted").unwrap();
        let BeginCoordinate::Execute(second) = workset
            .begin("terminal/fix-git|harness|high", Duration::from_secs(30))
            .unwrap()
        else {
            panic!("retry should execute");
        };

        assert_eq!(first.repetition, second.repetition);
        assert!(second.generation > first.generation);
    }

    #[test]
    fn unknown_family_never_expands_the_closed_profile() {
        let directory = tempfile::tempdir().unwrap();
        let workset = workset(directory.path(), 1);

        assert!(matches!(
            workset.begin("not-in-profile", Duration::from_secs(30)),
            Err(WorksetError::UnknownFamily(family)) if family == "not-in-profile"
        ));
        assert_eq!(workset.status().unwrap().coordinates.pending, 1);
    }

    #[test]
    fn future_schema_is_rejected_without_rewriting_its_version() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection.pragma_update(None, "user_version", 99).unwrap();
        drop(connection);

        assert!(matches!(
            Workset::open(&path, "release"),
            Err(WorksetError::DefinitionConflict(message)) if message.contains("schema 99")
        ));
        let connection = Connection::open(path).unwrap();
        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 99);
    }

    #[test]
    fn version_one_ledgers_gain_host_assignment_without_losing_work() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE worksets(
                    id INTEGER PRIMARY KEY,
                    profile TEXT NOT NULL,
                    digest TEXT NOT NULL,
                    config_path TEXT NOT NULL,
                    created_at_ms INTEGER NOT NULL,
                    UNIQUE(profile, digest)
                 );
                 CREATE TABLE tasks(
                    id INTEGER PRIMARY KEY,
                    workset_id INTEGER NOT NULL REFERENCES worksets(id),
                    selector TEXT NOT NULL,
                    name TEXT NOT NULL,
                    root TEXT NOT NULL,
                    digest TEXT NOT NULL,
                    preparation_state TEXT NOT NULL,
                    preparation_generation INTEGER NOT NULL,
                    preparation_owner TEXT,
                    preparation_expires_at_ms INTEGER,
                    preparation_error TEXT,
                    UNIQUE(workset_id, selector)
                 );
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        drop(connection);

        let workset = Workset::create(&path, "release").unwrap();
        let (tasks, families) = definition(directory.path(), 1);
        workset.append(&tasks, &families).unwrap();
        let BeginCoordinate::Prepare(preparation) = workset
            .begin_for_host(
                "terminal/fix-git|harness|high",
                "127.0.0.1",
                Duration::from_secs(30),
            )
            .unwrap()
        else {
            panic!("migrated task should remain claimable");
        };
        workset.complete_preparation(&preparation).unwrap();
        assert_eq!(
            workset.status().unwrap().families[0]
                .assigned_host
                .as_deref(),
            Some("127.0.0.1")
        );
        let connection = Connection::open(path).unwrap();
        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }
}
