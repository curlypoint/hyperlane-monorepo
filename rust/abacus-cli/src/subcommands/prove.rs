use std::{convert::TryFrom, sync::Arc};
use structopt::StructOpt;

use crate::{inboxes, rpc};

use abacus_core::{
    accumulator::merkle::Proof,
    db::{AbacusDB, DB},
    AbacusMessage, ContractLocator, Decode, Inbox, MessageStatus, Signers,
};
use abacus_ethereum::EthereumInbox;

use ethers::{
    prelude::{Http, Middleware, Provider, SignerMiddleware, H160},
    types::H256,
};

use color_eyre::{eyre::bail, Result};
use ethers_signers::{AwsSigner, Signer};

use once_cell::sync::OnceCell;
use rusoto_core::{credential::EnvironmentProvider, HttpClient};
use rusoto_kms::KmsClient;

static KMS_CLIENT: OnceCell<KmsClient> = OnceCell::new();

type ConcreteInbox = EthereumInbox<SignerMiddleware<Provider<Http>, Signers>>;

#[derive(StructOpt, Debug)]
pub struct ProveCommand {
    /// Leaf to prove
    #[structopt(long, required_unless = "leaf_index")]
    leaf: Option<H256>,

    /// Leaf index to prove
    #[structopt(long, required_unless = "leaf")]
    leaf_index: Option<u32>,

    /// The name of the outbox chain, used to lookup keys in the db
    #[structopt(long)]
    outbox_name: String,

    /// Path to db containing proof
    #[structopt(long)]
    db_path: String,

    /// HexKey to use (please be careful)
    #[structopt(long)]
    key: Option<String>,

    /// If using AWS signer, the key ID
    #[structopt(long)]
    key_id: Option<String>,

    /// If using AWS signer, the region
    #[structopt(long)]
    aws_region: Option<String>,

    /// inbox contract address
    #[structopt(long)]
    address: Option<String>,

    /// RPC connection details
    #[structopt(long)]
    rpc: Option<String>,
}

impl ProveCommand {
    pub async fn run(&self) -> Result<()> {
        let db = AbacusDB::new(&self.outbox_name, DB::from_path(&self.db_path)?);
        let (message, proof) = self.fetch_proof(db)?;
        let inbox = self.inbox(message.origin, message.destination).await?;

        let status = inbox
            .message_status(message.to_leaf(self.leaf_index.unwrap()))
            .await?;
        let outcome = match status {
            MessageStatus::None => inbox.process(&message, &proof).await?,
            _ => {
                println!("Message already processed.");
                return Ok(());
            }
        };

        println!("{:?}", outcome);
        Ok(())
    }

    // mostly copied from abacus-base settings
    async fn signer(&self) -> Result<Signers> {
        if let Some(key) = &self.key {
            Ok(Signers::Local(key.parse()?))
        } else {
            match (&self.key_id, &self.aws_region) {
                (Some(id), Some(region)) => {
                    let client = KMS_CLIENT.get_or_init(|| {
                        KmsClient::new_with_client(
                            rusoto_core::Client::new_with(
                                EnvironmentProvider::default(),
                                HttpClient::new().unwrap(),
                            ),
                            region.parse().expect("invalid region"),
                        )
                    });
                    let signer = AwsSigner::new(client, id, 0).await?;
                    Ok(Signers::Aws(signer))
                }

                _ => bail!("missing signer information"),
            }
        }
    }

    fn fetch_proof(&self, db: AbacusDB) -> Result<(AbacusMessage, Proof)> {
        let idx = match (self.leaf_index, self.leaf) {
            (Some(idx), _) => idx,
            (None, Some(digest)) => match db.message_by_leaf(digest)? {
                Some(leaf) => leaf.leaf_index,
                None => bail!("No leaf index or "),
            },
            (None, None) => bail!("Must provide leaf index or leaf hash"),
        };

        let proof = db.proof_by_leaf_index(idx)?.expect("no proof");
        let message = db.message_by_leaf_index(idx)?.expect("no message");
        let message = AbacusMessage::read_from(&mut message.message.as_slice())?;

        Ok((message, proof))
    }

    async fn inbox(&self, origin: u32, destination: u32) -> Result<ConcreteInbox> {
        // bit ugly. Tries passed-in rpc first, then defaults to lookup by
        // domain
        let provider = self
            .rpc
            .as_ref()
            .map(Provider::<Http>::try_from)
            .transpose()?
            .unwrap_or_else(|| rpc::fetch_rpc_connection(destination).unwrap());

        let chain_id = provider.get_chainid().await?;
        let signer = self.signer().await?.with_chain_id(chain_id.low_u64());
        let middleware = SignerMiddleware::new(provider, signer);

        // bit ugly. Tries passed-in address first, then defaults to lookup by
        // domain
        let address = self
            .address
            .as_ref()
            .map(|addr| addr.parse::<H160>())
            .transpose()?
            .unwrap_or_else(|| inboxes::address_by_domain_pair(origin, destination).unwrap());

        Ok(EthereumInbox::new(
            Arc::new(middleware),
            &ContractLocator {
                name: "".into(),
                domain: 0,
                address: address.into(),
            },
        ))
    }
}