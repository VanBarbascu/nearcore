use clap::{Parser, ValueEnum};
use near_chain_configs::GenesisValidationMode;
use near_store::{DBCol, Mode, NodeStorage, Store};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

#[derive(Parser)]
#[command(name = "transfuse")]
#[command(about = "Transfer blocks and block headers between NEAR node instances")]
pub struct TransfuseArgs {
    /// Source NEAR node home directory
    #[arg(long)]
    pub source_home: PathBuf,

    /// Destination NEAR node home directory  
    #[arg(long)]
    pub destination_home: PathBuf,

    /// Column type to transfer
    #[arg(short, long, value_enum, default_value_t = ColumnType::Both)]
    pub column_type: ColumnType,

    /// Start height for block transfer (inclusive)
    #[arg(short, long)]
    pub start_height: Option<u64>,

    /// End height for block transfer (exclusive, optional)
    #[arg(short, long)]
    pub end_height: Option<u64>,

    // Always copy mode - source is read-only to prevent corruption

    /// Batch size for processing
    #[arg(short, long, default_value_t = 1000)]
    pub batch_size: usize,

    /// Verify data integrity after transfer
    #[arg(short, long)]
    pub verify: bool,

    /// Show progress
    #[arg(short, long)]
    pub progress: bool,

    /// Skip existing blocks in destination
    #[arg(long)]
    pub skip_existing: bool,
}

#[derive(ValueEnum, Clone)]
pub enum ColumnType {
    /// Transfer both block headers and blocks
    Both,
    /// Transfer only block headers
    Headers,
    /// Transfer only blocks
    Blocks,
}

// Transfer mode removed - always copy to prevent source corruption

pub struct TransferStats {
    keys_transferred: usize,
    bytes_transferred: usize,
    start_time: Instant,
    height_range: Option<(u64, u64)>,
}

impl TransferStats {
    fn new() -> Self {
        Self {
            keys_transferred: 0,
            bytes_transferred: 0,
            start_time: Instant::now(),
            height_range: None,
        }
    }

    fn set_height_range(&mut self, start: u64, end: Option<u64>) {
        self.height_range = Some((start, end.unwrap_or(u64::MAX)));
    }

    fn add_key(&mut self, key_size: usize, value_size: usize) {
        self.keys_transferred += 1;
        self.bytes_transferred += key_size + value_size;
    }

    fn print_progress(&self, current_height: Option<u64>) {
        let elapsed = self.start_time.elapsed();
        let rate = if elapsed.as_secs() > 0 {
            self.keys_transferred as f64 / elapsed.as_secs() as f64
        } else {
            0.0
        };

        let mb_transferred = self.bytes_transferred as f64 / 1024.0 / 1024.0;
        let mb_rate = if elapsed.as_secs() > 0 {
            mb_transferred / elapsed.as_secs() as f64
        } else {
            0.0
        };

        if let Some((start, end)) = self.height_range {
            if let Some(current) = current_height {
                let total_blocks = end.saturating_sub(start);
                let processed_blocks = current.saturating_sub(start);
                let percentage = if total_blocks > 0 {
                    (processed_blocks as f64 / total_blocks as f64) * 100.0
                } else {
                    0.0
                };
                println!(
                    "Progress: {}/{} blocks ({:.1}%) | Height: {} | {:.2} MB | {:.0} keys/s | {:.2} MB/s",
                    processed_blocks, total_blocks, percentage, current, mb_transferred, rate, mb_rate
                );
            } else {
                println!(
                    "Transferred: {} keys | Height range: {}..{} | {:.2} MB | {:.0} keys/s | {:.2} MB/s",
                    self.keys_transferred, start, end, mb_transferred, rate, mb_rate
                );
            }
        } else {
            println!(
                "Transferred: {} keys | {:.2} MB | {:.0} keys/s | {:.2} MB/s",
                self.keys_transferred, mb_transferred, rate, mb_rate
            );
        }
    }

    pub fn get_summary(&self) -> (usize, usize, std::time::Duration) {
        (self.keys_transferred, self.bytes_transferred, self.start_time.elapsed())
    }
}

fn open_store(home_dir: &PathBuf, read_only: bool) -> anyhow::Result<Arc<Store>> {
    // Load the config from the home directory
    let config = nearcore::config::load_config(home_dir, GenesisValidationMode::UnsafeFast)?;
    
    // Open the node storage
    let opener = NodeStorage::opener(
        &config.config.store.path.as_ref().unwrap_or(&home_dir.join("data")),
        &config.config.store,
        None, // No archival config for simple transfuse
    );
    
    let node_storage = if read_only {
        opener.open_in_mode(Mode::ReadOnly)?
    } else {
        opener.open_in_mode(Mode::ReadWrite)?
    };
    
    let store = node_storage.get_hot_store();
    Ok(Arc::new(store))
}

fn height_to_key(height: u64) -> Vec<u8> {
    // Convert height to little-endian bytes (NEAR uses little-endian for BlockHeight keys)
    height.to_le_bytes().to_vec()
}

fn key_to_height(key: &[u8]) -> Option<u64> {
    if key.len() == 8 {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(key);
        Some(u64::from_le_bytes(bytes))
    } else {
        None
    }
}

fn transfer_column(
    args: &TransfuseArgs,
    source_store: &Store,
    dest_store: &Store,
    column: DBCol,
    start_height: u64,
    end_height: Option<u64>,
) -> anyhow::Result<TransferStats> {
    println!("Transferring column '{:?}' from height {} to {:?}", column, start_height, end_height);

    let mut stats = TransferStats::new();
    stats.set_height_range(start_height, end_height);
    
    let mut batch_count = 0;
    let mut store_update = dest_store.store_update();

    // For height-based columns, we need to iterate based on height
    if matches!(column, DBCol::BlockHeight) {
        let actual_end_height = end_height.unwrap_or(u64::MAX);
        let mut current_height = start_height;
        
        while current_height <= actual_end_height {
            let height_key = height_to_key(current_height);
            
            if let Some(value) = source_store.get(column, &height_key)? {
                store_update.insert(column, height_key.clone(), value.to_vec());
                stats.add_key(height_key.len(), value.len());
                batch_count += 1;

                // Write batch when it reaches the specified size
                if batch_count >= args.batch_size {
                    store_update.commit()?;
                    store_update = dest_store.store_update();
                    batch_count = 0;

                    if args.progress {
                        stats.print_progress(Some(current_height));
                    }
                }
            }
            current_height += 1;
        }
    } else {
        // For other columns, we'll need to find blocks by height first, then get them by hash
        let actual_end_height = end_height.unwrap_or(u64::MAX);
        let mut current_height = start_height;
        
        while current_height <= actual_end_height {
            let height_key = height_to_key(current_height);
            
            // Get block hash from height
            if let Some(block_hash_bytes) = source_store.get(DBCol::BlockHeight, &height_key)? {
                // Get the actual block/header data
                if let Some(data) = source_store.get(column, &block_hash_bytes)? {
                    store_update.insert(column, block_hash_bytes.to_vec(), data.to_vec());
                    stats.add_key(block_hash_bytes.len(), data.len());
                    batch_count += 1;

                    // Write batch when it reaches the specified size
                    if batch_count >= args.batch_size {
                        store_update.commit()?;
                        store_update = dest_store.store_update();
                        batch_count = 0;

                        if args.progress {
                            stats.print_progress(Some(current_height));
                        }
                    }
                }
            }
            current_height += 1;
        }
    }

    // Write remaining items in batch
    if batch_count > 0 {
        store_update.commit()?;
    }

    Ok(stats)
}

fn verify_transfer(
    source_store: &Store,
    dest_store: &Store,
    column: DBCol,
    start_height: u64,
    end_height: Option<u64>,
) -> anyhow::Result<()> {
    println!("Verifying transfer for column '{:?}'...", column);
    
    let start_key = height_to_key(start_height);
    let end_key = end_height.map(height_to_key);
    
    let mut source_iter = source_store.iter_range(column, Some(&start_key), end_key.as_deref());
    let mut dest_iter = dest_store.iter_range(column, Some(&start_key), end_key.as_deref());
    
    let mut verified = 0;
    let mut mismatches = 0;

    loop {
        let source_item = source_iter.next();
        let dest_item = dest_iter.next();

        match (source_item, dest_item) {
            (Some(Ok((sk, sv))), Some(Ok((dk, dv)))) => {
                if sk == dk && sv == dv {
                    verified += 1;
                } else {
                    mismatches += 1;
                    if mismatches <= 10 {
                        let height = key_to_height(&sk).unwrap_or(0);
                        eprintln!("Mismatch at height {} in column '{:?}'", height, column);
                    }
                }
            }
            (None, None) => break,
            _ => {
                mismatches += 1;
                eprintln!("Length mismatch in column '{:?}'", column);
                break;
            }
        }
    }

    println!("Verification complete for column '{:?}':", column);
    println!("  Keys verified: {}", verified);
    println!("  Mismatches: {}", mismatches);
    
    if mismatches > 0 {
        return Err(anyhow::anyhow!("Transfer verification failed for column '{:?}'", column));
    }

    Ok(())
}

pub fn run_transfuse(args: &TransfuseArgs) -> anyhow::Result<()> {
    println!("Opening source node: {:?}", args.source_home);
    let source_store = open_store(&args.source_home, true)?;

    println!("Opening destination node: {:?}", args.destination_home);
    let dest_store = open_store(&args.destination_home, false)?;

    let start_height = args.start_height.unwrap_or(0);
    let end_height = args.end_height;

    println!("Starting transfer from height {} to {:?}", start_height, end_height);

    let mut total_stats = TransferStats::new();
    total_stats.set_height_range(start_height, end_height);

    // Transfer based on column type
    match args.column_type {
        ColumnType::Both => {
            // Transfer block headers
            println!("\n=== Transferring Block Headers ===");
            let stats = transfer_column(args, &source_store, &dest_store, DBCol::BlockHeader, start_height, end_height)?;
            total_stats.keys_transferred += stats.keys_transferred;
            total_stats.bytes_transferred += stats.bytes_transferred;
            
            if args.verify {
                verify_transfer(&source_store, &dest_store, DBCol::BlockHeader, start_height, end_height)?;
            }

            // Transfer blocks
            println!("\n=== Transferring Blocks ===");
            let stats = transfer_column(args, &source_store, &dest_store, DBCol::Block, start_height, end_height)?;
            total_stats.keys_transferred += stats.keys_transferred;
            total_stats.bytes_transferred += stats.bytes_transferred;
            
            if args.verify {
                verify_transfer(&source_store, &dest_store, DBCol::Block, start_height, end_height)?;
            }
        }
        ColumnType::Headers => {
            println!("\n=== Transferring Block Headers ===");
            let stats = transfer_column(args, &source_store, &dest_store, DBCol::BlockHeader, start_height, end_height)?;
            total_stats.keys_transferred += stats.keys_transferred;
            total_stats.bytes_transferred += stats.bytes_transferred;
            
            if args.verify {
                verify_transfer(&source_store, &dest_store, DBCol::BlockHeader, start_height, end_height)?;
            }
        }
        ColumnType::Blocks => {
            println!("\n=== Transferring Blocks ===");
            let stats = transfer_column(args, &source_store, &dest_store, DBCol::Block, start_height, end_height)?;
            total_stats.keys_transferred += stats.keys_transferred;
            total_stats.bytes_transferred += stats.bytes_transferred;
            
            if args.verify {
                verify_transfer(&source_store, &dest_store, DBCol::Block, start_height, end_height)?;
            }
        }
    }

    // Final statistics
    let (keys, bytes, elapsed) = total_stats.get_summary();
    println!("\n=== Transfer Summary ===");
    println!("Total time: {:.2?}", elapsed);
    println!("Keys transferred: {}", keys);
    println!("Bytes transferred: {:.2} MB", bytes as f64 / 1024.0 / 1024.0);
    println!("Average rate: {:.0} keys/s", keys as f64 / elapsed.as_secs_f64());
    println!("Height range: {}..{:?}", start_height, end_height);

    Ok(())
}
