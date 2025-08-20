use near_chain_configs::{GenesisValidationMode};
use near_store::{DBCol, NodeStorage, Store};
use std::path::{Path, PathBuf};

#[derive(clap::Subcommand)]
enum BlockMiscKeySelector {
    StateSnapshot,
    HeaderHead,
}

#[derive(clap::Subcommand)]
enum ColumnSelector {
    BlockMisc {
        #[clap(subcommand)]
        key: BlockMiscKeySelector,
    },
}

#[derive(clap::Args)]
pub(crate) struct WriteCryptoHashCommand {
    #[clap(long)]
    hash: Option<near_primitives::hash::CryptoHash>,
    #[clap(long, conflicts_with = "hash")]
    clone_from_other_home_dir: Option<PathBuf>,
    #[clap(subcommand)]
    column: ColumnSelector,
}

impl WriteCryptoHashCommand {

    fn get_crypto_hash(&self, source_store: Option<&Store>, key: &[u8]) -> anyhow::Result<near_primitives::hash::CryptoHash> {
        if let Some(hash) = self.hash {
            return Ok(hash);
        }
        if let Some(source_store) = source_store {
            if let Some(value) = source_store.get_ser::<near_primitives::hash::CryptoHash>(DBCol::BlockMisc, key)? {
                return Ok(value);
            }
        }
        anyhow::bail!("No source store provided and no hash provided");
    }

    fn get_tip(&self, source_store: Option<&Store>, key: &[u8]) -> anyhow::Result<near_primitives::block::Tip> {
        if let Some(source_store) = source_store {
            if let Some(value) = source_store.get_ser::<near_primitives::block::Tip>(DBCol::BlockMisc, key)? {
                return Ok(value);
            }
        }
        anyhow::bail!("No source store provided for Tip");
    }

    pub(crate) fn run(
        &self,
        home_dir: &Path,
        genesis_validation: GenesisValidationMode,
    ) -> anyhow::Result<()> {
        let near_config = nearcore::config::load_config(&home_dir, genesis_validation)?;
        let opener = NodeStorage::opener(
            home_dir,
            &near_config.config.store,
            near_config.config.archival_config(),
        );

        let source_store = if let Some(clone_from_other_home_dir) = &self.clone_from_other_home_dir {
            let source_near_config = nearcore::config::load_config(&clone_from_other_home_dir, GenesisValidationMode::UnsafeFast)?;
            let source_opener = NodeStorage::opener(
                clone_from_other_home_dir,
                &source_near_config.config.store,
                source_near_config.config.archival_config(),
            );
            let source_storage = source_opener.open()?;
            let source_store = source_storage.get_hot_store();
            Some(source_store)
        } else { None };
       
        let storage = opener.open()?;
        let store = storage.get_hot_store();
        let mut store_update = store.store_update();

        match &self.column {
            ColumnSelector::BlockMisc { key } => match key {
                BlockMiscKeySelector::StateSnapshot => {
                    store_update.set_ser(
                        DBCol::BlockMisc,
                        near_store::STATE_SNAPSHOT_KEY,
                        &self.get_crypto_hash(source_store.as_ref(), near_store::STATE_SNAPSHOT_KEY)?,
                    )?;
                }
                BlockMiscKeySelector::HeaderHead => {
                    store_update.set_ser(
                        DBCol::BlockMisc,
                        near_store::HEADER_HEAD_KEY,
                        &self.get_tip(source_store.as_ref(), near_store::HEADER_HEAD_KEY)?,
                    )?;
                }
            },
        }

        Ok(store_update.commit()?)
    }
}
