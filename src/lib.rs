// SPDX-License-Identifier: BSD-3-Clause
use solana_program_test::{ProgramTest, ProgramTestContext};
use solana_sdk::signer::signers::Signers;
use solana_sdk::{program_pack::Pack, signature::Signer, transaction::Transaction};
use std::error::Error;
use std::fs::File;
use std::io::Write;
use std::str::FromStr;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::Keypair,
};

use tempfile::Builder;

use tokio::net::TcpStream;

use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};

mod helpers {
    use rand::{prelude::StdRng, SeedableRng};
    use sha2::{Digest, Sha256};
    use solana_sdk::signature::Keypair;

    pub fn keypair_from_data(data: &[u8]) -> Keypair {
        let mut hash = Sha256::default();
        hash.update(data);

        // panic here is probably fine since this should always be 32 bytes, regardless of user input
        let mut rng = StdRng::from_seed(hash.finalize()[..].try_into().unwrap());
        Keypair::generate(&mut rng)
    }
}

pub struct Challenge<R, W> {
    input: R,
    output: W,
    pub ctx: ProgramTestContext,
}

pub struct ChallengeBuilder<R, W> {
    input: R,
    output: W,
    pub builder: ProgramTest,
}

impl<R: AsyncBufRead + Unpin, W: AsyncWrite + Unpin> ChallengeBuilder<R, W> {
    async fn read_line(&mut self) -> Result<String, Box<dyn Error>> {
        let mut line = String::new();
        self.input.read_line(&mut line).await?;

        Ok(line.replace("\n", ""))
    }

    /// Build challenge environment
    pub async fn build(self) -> Challenge<R, W> {
        Challenge {
            input: self.input,
            output: self.output,
            ctx: self.builder.start_with_context().await,
        }
    }

    /// Adds programs to challenge environment
    ///
    /// Returns vector of program pubkeys, with positions corresponding to input slice
    pub fn add_program(&mut self, path: &str, key: Option<Pubkey>) -> Pubkey {
        let program_so = std::fs::read(path).unwrap();
        let program_key = key.unwrap_or(helpers::keypair_from_data(&program_so).pubkey());

        self.builder
            .add_program(&path.replace(".so", ""), program_key, None);

        program_key
    }

    /// Reads program from input and adds it to environment
    pub async fn input_program(&mut self) -> Result<Pubkey, Box<dyn Error>> {
        self.output.write_all(b"program pubkey: ").await?;
        self.output.flush().await?;
        let program_key = Pubkey::from_str(&self.read_line().await?)?;

        self.output.write_all(b"program len: ").await?;
        self.output.flush().await?;
        let len: usize = std::cmp::min(10_000_000, self.read_line().await?.parse()?);

        let mut input_so = vec![0; len];
        self.input.read_exact(&mut input_so).await?;

        let dir = Builder::new()
            .prefix("my-temporary-dir")
            .rand_bytes(5)
            .tempdir()?;

        let file_path = dir.path().join("solve.so");
        let mut input_file = File::create(file_path.clone())?;

        input_file.write_all(&input_so)?;

        self.add_program(file_path.to_str().unwrap(), Some(program_key));

        Ok(program_key)
    }
}

impl<R: AsyncBufRead + Unpin, W: AsyncWrite + Unpin> Challenge<R, W> {
    pub fn builder(input: R, output: W) -> ChallengeBuilder<R, W> {
        let mut builder = ProgramTest::default();
        builder.prefer_bpf(true);

        ChallengeBuilder {
            input,
            output,
            builder,
        }
    }

    pub async fn add_token_account(
        &mut self,
        mint: &Pubkey,
        owner: &Pubkey,
    ) -> Result<Pubkey, Box<dyn Error>> {
        let token_account_keypair = Keypair::new();
        let token_account = token_account_keypair.pubkey();
        let payer = self.ctx.payer.insecure_clone();
        let mut tx = Transaction::new_with_payer(
            &[
                solana_program::system_instruction::create_account(
                    &payer.pubkey(),
                    &token_account,
                    10000000,
                    spl_token::state::Account::LEN.try_into().unwrap(),
                    &spl_token::ID,
                ),
                spl_token::instruction::initialize_account(
                    &spl_token::ID,
                    &token_account,
                    mint,
                    owner,
                )?,
            ],
            Some(&payer.pubkey()),
        );
        tx.try_sign(
            &[&token_account_keypair, &payer],
            self.ctx.get_new_latest_blockhash().await?,
        )?;
        self.ctx
            .banks_client
            .process_transaction_with_preflight(tx)
            .await?;

        Ok(token_account)
    }

    pub async fn add_mint(&mut self) -> Result<Pubkey, Box<dyn Error>> {
        let mint_keypair = Keypair::new();
        let mint = mint_keypair.pubkey();
        let payer = self.ctx.payer.insecure_clone();
        let mut tx = Transaction::new_with_payer(
            &[
                solana_program::system_instruction::create_account(
                    &payer.pubkey(),
                    &mint,
                    10000000,
                    spl_token::state::Mint::LEN.try_into().unwrap(),
                    &spl_token::ID,
                ),
                spl_token::instruction::initialize_mint(
                    &spl_token::ID,
                    &mint,
                    &payer.pubkey(),
                    None,
                    9,
                )?,
            ],
            Some(&payer.pubkey()),
        );
        tx.try_sign(
            &[&mint_keypair, &payer],
            self.ctx.get_new_latest_blockhash().await?,
        )?;
        self.ctx
            .banks_client
            .process_transaction_with_preflight(tx)
            .await?;

        Ok(mint)
    }

    pub async fn mint_to(
        &mut self,
        amount: u64,
        mint: &Pubkey,
        account: &Pubkey,
    ) -> Result<(), Box<dyn Error>> {
        self.run_ix(spl_token::instruction::mint_to(
            &spl_token::ID,
            mint,
            account,
            &self.ctx.payer.pubkey(),
            &[],
            amount,
        )?)
        .await
    }

    pub async fn run_ixs(&mut self, ixs: &[Instruction]) -> Result<(), Box<dyn Error>> {
        let payer_keypair = self.ctx.payer.insecure_clone();
        let payer = payer_keypair.pubkey();
        let mut tx = Transaction::new_with_payer(ixs, Some(&payer));

        tx.try_sign(
            &[&payer_keypair],
            self.ctx.get_new_latest_blockhash().await?,
        )?;
        self.ctx
            .banks_client
            .process_transaction_with_preflight(tx)
            .await?;

        Ok(())
    }

    pub async fn run_ix(&mut self, ix: Instruction) -> Result<(), Box<dyn Error>> {
        self.run_ixs(&[ix]).await
    }

    pub async fn run_ixs_full<T: Signers>(
        &mut self,
        ixs: &[Instruction],
        signers: &T,
        payer: &Pubkey,
    ) -> Result<(), Box<dyn Error>> {
        let mut tx = Transaction::new_with_payer(ixs, Some(payer));

        tx.try_sign(signers, self.ctx.get_new_latest_blockhash().await?)?;
        self.ctx
            .banks_client
            .process_transaction_with_preflight(tx)
            .await?;

        Ok(())
    }

    pub async fn read_token_account(
        &mut self,
        pubkey: Pubkey,
    ) -> Result<spl_token::state::Account, Box<dyn Error>> {
        Ok(spl_token::state::Account::unpack(
            &self
                .ctx
                .banks_client
                .get_account(pubkey)
                .await?
                .unwrap()
                .data,
        )?)
    }

    /// Reads instruction accounts/data from input.
    ///
    /// If program_id is None, will ask user for a pubkey.
    ///
    /// # Account Format:
    /// `[meta] [pubkey]`
    ///
    /// `[meta]` - contains "s" if account is a signer, "w" if it is writable
    /// `[pubkey]` - the address of the account
    pub async fn read_instruction(
        &mut self,
        program_id: Option<Pubkey>,
    ) -> Result<Instruction, Box<dyn Error>> {
        let mut line = String::new();
        let program_id = match program_id {
            Some(id) => id,
            None => {
                self.output.write_all(b"program id: ").await?;
                self.output.flush().await?;
                self.input.read_line(&mut line).await?;
                let id = Pubkey::from_str(line.trim())?;
                line.clear();
                id
            }
        };
        self.output.write_all(b"num accounts: ").await?;
        self.output.flush().await?;
        self.input.read_line(&mut line).await?;
        let num_accounts: usize = line.trim().parse()?;

        let mut metas = vec![];
        for _ in 0..num_accounts {
            line.clear();
            self.input.read_line(&mut line).await?;

            let mut it = line.trim().split(' ');
            let meta = it.next().ok_or("bad meta")?;
            let pubkey = it.next().ok_or("bad pubkey")?;
            let pubkey = Pubkey::from_str(pubkey)?;

            let is_signer = meta.contains('s');
            let is_writable = meta.contains('w');

            if is_writable {
                metas.push(AccountMeta::new(pubkey, is_signer));
            } else {
                metas.push(AccountMeta::new_readonly(pubkey, is_signer));
            }
        }

        line.clear();
        self.output.write_all(b"ix len: ").await?;
        self.output.flush().await?;
        self.input.read_line(&mut line).await?;
        let ix_data_len: usize = line.trim().parse()?;
        let mut ix_data = vec![0; ix_data_len];

        self.input.read_exact(&mut ix_data).await?;

        let ix = Instruction::new_with_bytes(program_id, &ix_data, metas);

        Ok(ix)
    }

    /// Reads a user-specified number of instructions with the same format as `read_instruction`.
    pub async fn read_instructions(
        &mut self,
        program_id: Option<Pubkey>,
    ) -> Result<Vec<Instruction>, Box<dyn Error>> {
        let mut ret = Vec::new();
        let mut line = String::new();
        self.output.write_all(b"ix count: ").await?;
        self.output.flush().await?;
        self.input.read_line(&mut line).await?;
        let ix_count: usize = line.trim().parse()?;
        for _ in 0..ix_count.min(256) {
            ret.push(self.read_instruction(program_id).await?);
        }
        Ok(ret)
    }
}

impl TryFrom<TcpStream> for ChallengeBuilder<BufReader<OwnedReadHalf>, OwnedWriteHalf> {
    type Error = std::io::Error;

    fn try_from(socket: TcpStream) -> Result<Self, Self::Error> {
        let (reader, writer) = socket.into_split();
        let bufreader = BufReader::new(reader);
        Ok(Challenge::builder(bufreader, writer))
    }
}
