---- MODULE SFFFinality ----
(***************************************************************************)
(* Sunchain Fast Finality (SFF) - F1 formal state-machine model.            *)
(*                                                                         *)
(* This spec formalises the SAFETY invariants that the F1 implementation in *)
(* `crates/sff-verifier` (LockTracker + verify_certificate_logic +          *)
(* equivocation detection) must preserve. The executable, CI-verifiable     *)
(* counterpart is the bounded model checker in `sff-verifier/src/lib.rs`    *)
(* (`mod model_check`), which enumerates small committees and exercises     *)
(* every reachable LockTracker state against these same invariants.         *)
(*                                                                         *)
(* Key design points mirrored here (design doc, SFF Fast Finality, §4.3-4.4):*)
(*   * Votes / certificates are threshold objects: a valid certificate has  *)
(*     STRICTLY MORE THAN 2/3 of the total weighted stake (> 2/3).          *)
(*   * Two conflicting certificates (> 2/3 each) at the same (height,       *)
(*     round, phase) must therefore share > 1/3 of the stake, i.e. any      *)
(*     safety breach requires > 1/3 double-signing - impossible under the   *)
(*     < 1/3 Byzantine assumption. This is the `SafetyInv` theorem.        *)
(*   * A Commit certificate may only exist if a Prepare certificate for the *)
(*     same (height, round, block) was recorded first (two-phase lock).     *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    Nodes,      \* finite set of authority identifiers
    Stake,      \* [Nodes -> Nat \ {0}] : stake of each authority
    Blocks,     \* finite set of possible block hashes (model: {b0, b1})
    Heights,    \* finite set of heights to explore
    Rounds      \* finite set of rounds to explore

ASSUME
    /\ IsFiniteSet(Nodes) /\ Nodes # {}
    /\ IsFiniteSet(Blocks)
    /\ IsFiniteSet(Heights)
    /\ IsFiniteSet(Rounds)
    /\ \A n \in Nodes : Stake[n] \in (Nat \ {0})

totalStake == SumSet({Stake[n] : n \in Nodes})

\* A certificate is a quorum iff its signed stake is strictly greater than 2/3.
QUORUM(signed) == 3 * signed > 2 * totalStake

\* "strictly more than 1/3" - the minimum overlap of two conflicting quorums.
MORE_THAN_THIRD(s) == 3 * s > totalStake

SignerSet == { S \in SUBSET Nodes : QUORUM(SumSet({Stake[n] : n \in S})) }

VARIABLES
    prepareQCs,     \* set of valid Prepare certificates observed on the network
    commitQCs       \* set of valid Commit certificates observed on the network

TypeOK ==
    /\ \A p \in prepareQCs :
        /\ p.h \in Heights /\ p.r \in Rounds /\ p.b \in Blocks
        /\ p.S \in SignerSet
    /\ \A c \in commitQCs :
        /\ c.h \in Heights /\ c.r \in Rounds /\ c.b \in Blocks
        /\ c.S \in SignerSet
        /\ c.prep \in prepareQCs
        /\ c.prep.h = c.h /\ c.prep.r = c.r /\ c.prep.b = c.b

QuorumInv ==
    /\ \A p \in prepareQCs : QUORUM(SumSet({Stake[n] : n \in p.S}))
    /\ \A c \in commitQCs   : QUORUM(SumSet({Stake[n] : n \in c.S}))

\* Two-phase lock: every commit references a matching prepare for the same
\* (height, round, block).
LockInv ==
    \A c \in commitQCs : \E p \in prepareQCs :
        /\ p.h = c.h /\ p.r = c.r /\ p.b = c.b /\ p = c.prep

\* SAFETY: two conflicting commit certificates at the same (height, round)
\* with different blocks must share > 1/3 of the stake. Under < 1/3 Byzantine
\* this is impossible, so conflicting commits cannot both be valid.
SafetyInv ==
    \A c1, c2 \in commitQCs :
        (c1.h = c2.h /\ c1.r = c2.r /\ c1.b # c2.b)
            => MORE_THAN_THIRD(SumSet({Stake[n] : n \in (c1.S \cap c2.S)}))

MakePrepare(h, r, b, S) ==
    /\ h \in Heights /\ r \in Rounds /\ b \in Blocks /\ S \in SignerSet
    /\ prepareQCs' = prepareQCs \cup {[h |-> h, r |-> r, b |-> b, S |-> S]}
    /\ UNCHANGED commitQCs

MakeCommit(p, b, S) ==
    /\ p \in prepareQCs
    /\ b \in Blocks /\ S \in SignerSet
    /\ b = p.b
    /\ commitQCs' = commitQCs \cup {[h |-> p.h, r |-> p.r, b |-> b, S |-> S, prep |-> p]}
    /\ UNCHANGED prepareQCs

Init == /\ prepareQCs = {} /\ commitQCs = {}

Next ==
    \/ \E h \in Heights, r \in Rounds, b \in Blocks, S \in SignerSet : MakePrepare(h, r, b, S)
    \/ \E p \in prepareQCs, b \in Blocks, S \in SignerSet : MakeCommit(p, b, S)

Spec == Init /\ [][Next]_<<prepareQCs, commitQCs>>

THEOREM Spec => []TypeOK
THEOREM Spec => []SafetyInv
THEOREM Spec => []LockInv

====
