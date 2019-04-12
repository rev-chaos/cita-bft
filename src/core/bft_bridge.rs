// CITA
// Copyright 2016-2017 Cryptape Technologies LLC.

// This program is free software: you can redistribute it
// and/or modify it under the terms of the GNU General Public
// License as published by the Free Software Foundation,
// either version 3 of the License, or (at your option) any
// later version.

// This program is distributed in the hope that it will be
// useful, but WITHOUT ANY WARRANTY; without even the implied
// warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR
// PURPOSE. See the GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::convert::{From, Into};

use bincode::{serialize, Infinite};
use crate::types::{H256, Address};
use crate::core::params::PrivateKey;
use crypto::{pubkey_to_address, Signature, Sign, SIGNATURE_BYTES_LEN, Signer};
use pubsub::channel::{Receiver, Sender, RecvError, select};
use proof::BftProof;
use bft::{BftMsg, BftSupport, Commit, Signature as BftSig, Address as BftAddr, Status, Node, Proof, BftActuator};
use hashable::Hashable;
use libproto::blockchain::{Block, Proof as ProtoProof, ProofType, BlockTxs};
use libproto::router::{MsgType, RoutingKey, SubModules};
use libproto::{TryFrom, TryInto, Message, auth, auth::VerifyBlockResp};
use std::collections::{HashMap, VecDeque};

use engine::{unix_now, AsMillis};

pub type PubType = (String, Vec<u8>);

pub enum BridgeMsg{
    CheckBlockReq(Vec<u8>, u64),
    CheckBlockResp(bool),
    CheckTxReq(Vec<u8>, u64, u64),
    CheckTxResp(bool),
    Transmit(BftMsg),
    Commit(Commit),
    GetBlockReq(u64),
    GetBlockResp(Option<Vec<u8>>),
    SignReq(Vec<u8>),
    SignResp(Option<BftSig>),
}

pub struct Processor {
    p2b_b: Sender<BridgeMsg>,
    p2b_f: Sender<BridgeMsg>,
    p2b_s: Sender<BridgeMsg>,
    p2b_t: Sender<BridgeMsg>,
    p2r: Sender<PubType>,
    p4b: Receiver<BridgeMsg>,
    p4r: Receiver<PubType>,
    bft_actuator: BftActuator,

    signer: PrivateKey,
    address: BftAddr,

    proof: HashMap<u64, Proof>,
    pre_hash: HashMap<u64, H256>,
    version:  HashMap<u64, u32>,

    get_block_reqs: VecDeque<u64>,
    check_tx_reqs: VecDeque<(u64, u64)>,

    get_block_resps: HashMap<u64, BlockTxs>,
    check_tx_resps: HashMap<(u64, u64), VerifyBlockResp>,

    is_snapshot: bool,
    is_cleared: bool,
}



impl Processor{
    pub fn start(&mut self){
        loop{
            let mut get_rab_msg = Err(RecvError);
            let mut get_bridge_msg = Err(RecvError);

            select! {
                recv(self.p4r) -> msg => get_rab_msg = msg,
                recv(self.p4b) -> msg => get_bridge_msg = msg,
            }

            if let Ok((key, body)) = get_rab_msg {
                let rt_key = RoutingKey::from(&key);
                match rt_key {
                    routing_key!(Net >> CompactSignedProposal) => {
                        self.bft_actuator.send(BftMsg::Proposal(body)).unwrap();
                    }

                    routing_key!(Net >> RawBytes) => {
                        self.bft_actuator.send(BftMsg::Vote(body)).unwrap();
                    }

                    routing_key!(Chain >> RichStatus) => {
                        let status = self.extract_status(&body[..]);
                        self.bft_actuator.send(BftMsg::Status(status)).unwrap();
                    }

                    routing_key!(Auth >> BlockTxs) => {
                        let mut msg = Message::try_from(&body[..]).unwrap();
                        let block_txs = msg.take_block_txs().unwrap();
                        self.get_block_resps.entry(block_txs.get_height()).or_insert(block_txs);
                        if let Some(req_height) = self.get_block_reqs.front(){
                            if let Some(block_txs) = self.get_block_resps.get(req_height) {
                                self.p2b_f.send(BridgeMsg::GetBlockResp(self.get_block(*req_height, block_txs))).unwrap();
                                self.get_block_reqs.pop_front();
                            }
                        }
                    }

                    routing_key!(Auth >> VerifyBlockResp) => {
//                        self.resp_sender.send((key, body)).unwrap();
                    }

                    routing_key!(Snapshot >> SnapshotReq) => {
                        // TODO
                    }

                    _ => {}
                }
            }

            if let Ok(bridge_msg) = get_bridge_msg {
                match bridge_msg{
                    BridgeMsg::GetBlockReq(height) => {
                        self.get_block_reqs.push_back(height);
                    }

                    BridgeMsg::CheckBlockReq(block, height) => {
                        self.p2b_b.send(BridgeMsg::CheckBlockResp(self.check_block(&block, height))).unwrap();
                    }

                    BridgeMsg::CheckTxReq(block, height, round) => {
                        let msg = get_block_req_msg(&block, height, round);
                        self.p2r
                            .send((
                                routing_key!(Consensus >> VerifyBlockReq).into(),
                                msg.clone().try_into().unwrap(),
                            ))
                            .unwrap();
                        self.check_tx_reqs.push_back((height, round));
                    }

                    BridgeMsg::SignReq(hash) => {
                        self.p2b_s.send(BridgeMsg::SignResp(self.sign(&hash))).unwrap();
                    }

                    BridgeMsg::Transmit(bft_msg) => {
                        self.transmit(bft_msg);
                    }

                    BridgeMsg::Commit(commit) => {
                        self.commit(commit);
                    }

                    _ => {}
                }
            }
        }
    }

    pub fn new(p2b_b: Sender<BridgeMsg>,
               p2b_f: Sender<BridgeMsg>,
               p2b_s: Sender<BridgeMsg>,
               p2b_t: Sender<BridgeMsg>,
               p2r: Sender<PubType>,
               p4b: Receiver<BridgeMsg>,
               p4r: Receiver<PubType>,
               bft_actuator: BftActuator,
               pk: PrivateKey) -> Self{
        let signer = Signer::from(pk.signer.clone());
        let address = signer.address.to_vec();
        Processor{
            p2b_b,
            p2b_f,
            p2b_s,
            p2b_t,
            p2r,
            p4b,
            p4r,
            bft_actuator,
            signer: pk,
            address,
            proof: HashMap::new(),
            pre_hash: HashMap::new(),
            version: HashMap::new(),
            get_block_reqs: VecDeque::new(),
            check_tx_reqs: VecDeque::new(),
            get_block_resps: HashMap::new(),
            check_tx_resps: HashMap::new(),
            is_snapshot: false,
            is_cleared: false,
        }
    }

    fn check_block(&self, _block: &[u8], _height: u64) -> bool{
        true
    }
    /// A function to check signature.
    fn check_transaction(&mut self, _block: &[u8], _height: u64, _round: u64) -> bool{
//        loop{
//            let (_, body) = self.resp_receiver.recv().unwrap();
//            let mut msg = Message::try_from(body).unwrap();
//            let resp = msg.take_verify_block_resp().unwrap();
//            let block = resp.get_block();
//            let v_height = resp.get_height();
//            let v_round = resp.get_round();
//            if v_height == height && v_round == round {
//
//            } else {
//
//            }
//        }

        false
    }
    /// A funciton to transmit messages.
    fn transmit(&self, msg: BftMsg){
        match msg{
            BftMsg::Proposal(encode) => {
                self.p2r
                    .send((
                        routing_key!(Consensus >> CompactSignedProposal).into(),
                        encode,
                    ))
                    .unwrap();
            }

            BftMsg::Vote(encode) => {
                self.p2r
                    .send((
                        routing_key!(Consensus >> RawBytes).into(),
                        encode,
                    ))
                    .unwrap();
            }

            _ => warn!("transmit wrong msg type!"),
        }
    }
    /// A function to commit the proposal.
    fn commit(&mut self, _commit: Commit){

    }

    fn get_block (&self, height: u64, block_txs: &BlockTxs) -> Option<Vec<u8>>{
        let version = self.version.get(&height);
        let pre_hash = self.pre_hash.get(&height);
        let proof = self.proof.get(&height);
        if version.is_none() || pre_hash.is_none() || proof.is_none(){
            return None;
        }
        let mut block = Block::new();
        block.set_version(*version.unwrap());
        block.set_body(block_txs.clone().take_body());
        block.mut_header().set_prevhash(pre_hash.unwrap().0.to_vec());
        let bft_proof = to_bft_proof(proof.unwrap());
        block.mut_header().set_proof(bft_proof);
        let block_time = unix_now();
        block.mut_header().set_timestamp(AsMillis::as_millis(&block_time));
        block.mut_header().set_height(height);
        let transactions_root = block.get_body().transactions_root();
        block.mut_header().set_transactions_root(transactions_root.to_vec());
        block.mut_header().set_proposer(self.address.clone());
        let blk = block.clone().compact().try_into().unwrap();
        return Some(blk);
    }

    fn sign(&self, hash: &[u8]) -> Option<BftSig>{
        if let Ok(signature) = Signature::sign(&self.signer.signer, &H256::from(hash)){
            return Some((&signature.0).to_vec());
        }
        None
    }

    fn extract_status(&mut self, body: &[u8]) -> Status{
        let mut msg = Message::try_from(body).unwrap();
        let status = msg.take_rich_status().unwrap();
        let height = status.height;

        let pre_hash = H256::from_slice(&status.hash);
        self.pre_hash.entry(height).or_insert(pre_hash);
        self.version.entry(height).or_insert(status.version);

        let mut map = HashMap::new();
        status.get_nodes().iter().for_each(|node| {
            let counter = map.entry(node.to_vec()).or_insert(0u32);
            *counter += 1;
        });

        let authority_list: Vec<Node> = map.into_iter().map(|(node, n)|{
            Node{
                address: node,
                proposal_weight: n,
                vote_weight: 1,
            }
        }).collect();

        Status{
            height,
            interval: Some(status.interval),
            authority_list,
        }
    }
}

pub struct BftBridge {
    b2p: Sender<BridgeMsg>,
    b4p_b: Receiver<BridgeMsg>,
    b4p_f: Receiver<BridgeMsg>,
    b4p_s: Receiver<BridgeMsg>,
    b4p_t: Receiver<BridgeMsg>,
}

impl BftBridge {
    pub fn new(b2p: Sender<BridgeMsg>,
               b4p_b: Receiver<BridgeMsg>,
               b4p_f: Receiver<BridgeMsg>,
               b4p_s: Receiver<BridgeMsg>,
               b4p_t: Receiver<BridgeMsg>
    ) -> Self{
        BftBridge{
            b2p,
            b4p_b,
            b4p_f,
            b4p_s,
            b4p_t,
        }
    }
}

impl BftSupport for BftBridge {
    fn check_block(&self, block: &[u8], height: u64) -> bool{
        self.b2p.send(BridgeMsg::CheckBlockReq(block.to_vec(), height)).unwrap();
        if let BridgeMsg::CheckBlockResp(is_pass) = self.b4p_b.recv().unwrap(){
            return is_pass;
        }
        false
    }
    /// A function to check signature.
    fn check_transaction(&self, block: &[u8], height: u64, round: u64) -> bool{
        self.b2p.send(BridgeMsg::CheckTxReq(block.to_vec(), height, round)).unwrap();
        if let BridgeMsg::CheckTxResp(is_pass) = self.b4p_t.recv().unwrap(){
            return is_pass;
        }
        false
    }
    /// A funciton to transmit messages.
    fn transmit(&self, msg: BftMsg){
        self.b2p.send(BridgeMsg::Transmit(msg)).unwrap();
    }
    /// A function to commit the proposal.
    fn commit(&self, commit: Commit){
        self.b2p.send(BridgeMsg::Commit(commit)).unwrap();
    }

    fn get_block(&self, height: u64) -> Option<Vec<u8>>{
        self.b2p.send(BridgeMsg::GetBlockReq(height)).unwrap();
        if let BridgeMsg::GetBlockResp(block) = self.b4p_f.recv().unwrap(){
            return block;
        }
        None
    }

    fn sign(&self, hash: &[u8]) -> Option<BftSig>{
        self.b2p.send(BridgeMsg::SignReq(hash.to_vec())).unwrap();
        if let BridgeMsg::SignResp(sign) = self.b4p_s.recv().unwrap(){
            return sign;
        }
        None
    }

    fn check_sig(&self, signature: &[u8], hash: &[u8]) -> Option<BftAddr>{
        if signature.len() != SIGNATURE_BYTES_LEN {
            return None;
        }
        let signature = Signature::from(signature);
        if let Ok(pubkey) = signature.recover(&H256::from(hash)) {
            let address = pubkey_to_address(&pubkey);
            return Some(address.to_vec());
        }
        None
    }

    fn crypt_hash(&self, msg: &[u8]) -> Vec<u8>{
        msg.to_vec().crypt_hash().to_vec()
    }
}

fn to_bft_proof(proof: &Proof) -> ProtoProof {
    let commits: HashMap<Address, Signature> = proof.precommit_votes.iter()
        .map(|(addr, sig)|{
            (Address::from(&addr[..]), Signature::from(&sig[..]))
        }).collect();
    let bft_proof = BftProof{
        proposal: H256::from(&proof.block_hash[..]),
        height: proof.height as usize,
        round: proof.round as usize,
        commits,
    };
    let mut proof = ProtoProof::new();
    let encoded_proof: Vec<u8> = serialize(&bft_proof, Infinite).unwrap();
    proof.set_content(encoded_proof);
    proof.set_field_type(ProofType::Bft);
    proof
}

fn get_block_req_msg (block: &[u8], height: u64, round: u64) -> Message{
    let mut msg = Message::try_from(block).unwrap();
    let origin = msg.get_origin();
    let compact_block = msg.take_compact_block().unwrap();
    let mut verify_req = auth::VerifyBlockReq::new();
    verify_req.set_height(height);
    verify_req.set_round(round);
    verify_req.set_block(compact_block);
    let mut msg: Message = verify_req.into();
    msg.set_origin(origin);
    msg
}


#[cfg(test)]
mod test {
    use super::*;
    use bft::{Status, Node};
    use std::collections::HashMap;

    #[test]
    fn test_extract_status() {
        let rich_status = vec![vec![5u8], vec![4u8], vec![4u8], vec![7u8], vec![9u8],
                               vec![9u8], vec![4u8], vec![1u8], vec![9u8], vec![7u8],
                               vec![9u8], vec![8u8], vec![7u8], vec![9u8], vec![9u8],];

        let mut map = HashMap::new();
        rich_status.iter().for_each(|node| {
            let counter = map.entry(node.to_vec()).or_insert(0u32);
            *counter += 1;
        });

        let authority_list: Vec<Node> = map.into_iter().map(|(node, n)|{
            Node{
                address: node,
                proposal_weight: n,
                vote_weight: 1,
            }
        }).collect();

        println!("{:?}", authority_list);
    }
}