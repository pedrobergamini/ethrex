// SPDX-License-Identifier: MIT
pragma solidity =0.8.29;

import "@openzeppelin/contracts-upgradeable/proxy/utils/UUPSUpgradeable.sol";
import "@openzeppelin/contracts-upgradeable/proxy/utils/Initializable.sol";
import "@openzeppelin/contracts-upgradeable/access/OwnableUpgradeable.sol";
import "./interfaces/IOnChainProposer.sol";
import {CommonBridge} from "./CommonBridge.sol";
import {ICommonBridge} from "./interfaces/ICommonBridge.sol";
import {IRiscZeroVerifier} from "./interfaces/IRiscZeroVerifier.sol";
import {ISP1Verifier} from "./interfaces/ISP1Verifier.sol";
import {IPicoVerifier} from "./interfaces/IPicoVerifier.sol";

/// @title OnChainProposer contract.
/// @author LambdaClass
contract OnChainProposer is
    IOnChainProposer,
    Initializable,
    UUPSUpgradeable,
    OwnableUpgradeable
{
    /// @notice Committed batches data.
    /// @dev This struct holds the information about the committed batches.
    /// @dev processedDepositLogsRollingHash is the Merkle root of the logs of the
    /// deposits that were processed in the batch being committed. The amount of
    /// logs that is encoded in this root are to be removed from the
    /// pendingDepositLogs queue of the CommonBridge contract.
    /// @dev withdrawalsLogsMerkleRoot is the Merkle root of the Merkle tree containing
    /// all the withdrawals that were processed in the batch being committed
    struct BatchCommitmentInfo {
        bytes32 newStateRoot;
        bytes32 stateDiffKZGVersionedHash;
        bytes32 processedDepositLogsRollingHash;
        bytes32 withdrawalsLogsMerkleRoot;
    }

    /// @notice The commitments of the committed batches.
    /// @dev If a batch is committed, the commitment is stored here.
    /// @dev If a batch was not committed yet, it won't be here.
    /// @dev It is used by other contracts to verify if a batch was committed.
    /// @dev The key is the batch number.
    mapping(uint256 => BatchCommitmentInfo) public batchCommitments;

    /// @notice The latest verified batch number.
    /// @dev This variable holds the batch number of the most recently verified batch.
    /// @dev All batches with a batch number less than or equal to `lastVerifiedBatch` are considered verified.
    /// @dev Batches with a batch number greater than `lastVerifiedBatch` have not been verified yet.
    /// @dev This is crucial for ensuring that only valid and confirmed batches are processed in the contract.
    uint256 public lastVerifiedBatch;

    /// @notice The latest committed batch number.
    /// @dev This variable holds the batch number of the most recently committed batch.
    /// @dev All batches with a batch number less than or equal to `lastCommittedBatch` are considered committed.
    /// @dev Batches with a block number greater than `lastCommittedBatch` have not been committed yet.
    /// @dev This is crucial for ensuring that only subsequents batches are committed in the contract.
    uint256 public lastCommittedBatch;

    /// @dev The sequencer addresses that are authorized to commit and verify batches.
    mapping(address _authorizedAddress => bool)
        public authorizedSequencerAddresses;

    address public BRIDGE;
    address public PICOVERIFIER;
    address public R0VERIFIER;
    address public SP1VERIFIER;

    /// @notice Address used to avoid the verification process.
    /// @dev If the `R0VERIFIER` or the `SP1VERIFIER` contract address is set to this address,
    /// the verification process will not happen.
    /// @dev Used only in dev mode.
    address public constant DEV_MODE = address(0xAA);

    /// @notice Indicates whether the contract operates in validium mode.
    /// @dev This value is immutable and can only be set during contract deployment.
    bool public VALIDIUM;

    modifier onlySequencer() {
        require(
            authorizedSequencerAddresses[msg.sender],
            "OnChainProposer: caller is not the sequencer"
        );
        _;
    }

    /// @notice Initializes the contract.
    /// @dev This method is called only once after the contract is deployed.
    /// @dev It sets the bridge address.
    /// @param _validium initialize the contract in validium mode.
    /// @param owner the address of the owner who can perform upgrades.
    /// @param r0verifier the address of the risc0 groth16 verifier.
    /// @param sp1verifier the address of the sp1 groth16 verifier.
    function initialize(
        bool _validium,
        address owner,
        address r0verifier,
        address sp1verifier,
        address picoverifier,
        bytes32 genesisStateRoot,
        address[] calldata sequencerAddresses
    ) public initializer {
        VALIDIUM = _validium;

        // Set the PicoGroth16Verifier address
        require(
            PICOVERIFIER == address(0),
            "OnChainProposer: contract already initialized"
        );
        require(
            picoverifier != address(0),
            "OnChainProposer: picoverifier is the zero address"
        );
        require(
            picoverifier != address(this),
            "OnChainProposer: picoverifier is the contract address"
        );
        PICOVERIFIER = picoverifier;

        // Set the Risc0Groth16Verifier address
        require(
            R0VERIFIER == address(0),
            "OnChainProposer: contract already initialized"
        );
        require(
            r0verifier != address(0),
            "OnChainProposer: r0verifier is the zero address"
        );
        require(
            r0verifier != address(this),
            "OnChainProposer: r0verifier is the contract address"
        );
        R0VERIFIER = r0verifier;

        // Set the SP1Groth16Verifier address
        require(
            SP1VERIFIER == address(0),
            "OnChainProposer: contract already initialized"
        );
        require(
            sp1verifier != address(0),
            "OnChainProposer: sp1verifier is the zero address"
        );
        require(
            sp1verifier != address(this),
            "OnChainProposer: sp1verifier is the contract address"
        );
        SP1VERIFIER = sp1verifier;

        batchCommitments[0] = BatchCommitmentInfo(
            genesisStateRoot,
            bytes32(0),
            bytes32(0),
            bytes32(0)
        );

        for (uint256 i = 0; i < sequencerAddresses.length; i++) {
            authorizedSequencerAddresses[sequencerAddresses[i]] = true;
        }

        OwnableUpgradeable.__Ownable_init(owner);
    }

    /// @inheritdoc IOnChainProposer
    function initializeBridgeAddress(address bridge) public onlyOwner {
        require(
            BRIDGE == address(0),
            "OnChainProposer: bridge already initialized"
        );
        require(
            bridge != address(0),
            "OnChainProposer: bridge is the zero address"
        );
        require(
            bridge != address(this),
            "OnChainProposer: bridge is the contract address"
        );
        BRIDGE = bridge;
    }

    /// @inheritdoc IOnChainProposer
    function commitBatch(
        uint256 batchNumber,
        bytes32 newStateRoot,
        bytes32 stateDiffKZGVersionedHash,
        bytes32 withdrawalsLogsMerkleRoot,
        bytes32 processedDepositLogsRollingHash
    ) external override onlySequencer {
        // TODO: Refactor validation
        require(
            batchNumber == lastCommittedBatch + 1,
            "OnChainProposer: batchNumber is not the immediate successor of lastCommittedBatch"
        );
        require(
            batchCommitments[batchNumber].newStateRoot == bytes32(0),
            "OnChainProposer: tried to commit an already committed batch"
        );

        // Check if commitment is equivalent to blob's KZG commitment.

        if (processedDepositLogsRollingHash != bytes32(0)) {
            bytes32 claimedProcessedDepositLogs = ICommonBridge(BRIDGE)
                .getPendingDepositLogsVersionedHash(
                    uint16(bytes2(processedDepositLogsRollingHash))
                );
            require(
                claimedProcessedDepositLogs == processedDepositLogsRollingHash,
                "OnChainProposer: invalid deposit logs"
            );
        }
        if (withdrawalsLogsMerkleRoot != bytes32(0)) {
            ICommonBridge(BRIDGE).publishWithdrawals(
                batchNumber,
                withdrawalsLogsMerkleRoot
            );
        }
        batchCommitments[batchNumber] = BatchCommitmentInfo(
            newStateRoot,
            stateDiffKZGVersionedHash,
            processedDepositLogsRollingHash,
            withdrawalsLogsMerkleRoot
        );
        emit BatchCommitted(newStateRoot);

        lastCommittedBatch = batchNumber;
    }

    /// @inheritdoc IOnChainProposer
    /// @notice The first `require` checks that the batch number is the subsequent block.
    /// @notice The second `require` checks if the batch has been committed.
    /// @notice The order of these `require` statements is important.
    /// Ordering Reason: After the verification process, we delete the `batchCommitments` for `batchNumber - 1`. This means that when checking the batch,
    /// we might get an error indicating that the batch hasn’t been committed, even though it was committed but deleted. Therefore, it has already been verified.
    function verifyBatch(
        uint256 batchNumber,
        //risc0
        bytes calldata risc0BlockProof,
        bytes32 risc0ImageId,
        //sp1
        bytes32 sp1ProgramVKey,
        bytes calldata sp1ProofBytes,
        //pico
        bytes32 picoRiscvVkey,
        uint256[8] calldata picoProof
    ) external override onlySequencer {
        // TODO: Refactor validation
        // TODO: imageid, programvkey and riscvvkey should be constants
        // TODO: organize each zkvm proof arguments in their own structs
        require(
            batchNumber == lastVerifiedBatch + 1,
            "OnChainProposer: batch already verified"
        );
        require(
            batchCommitments[batchNumber].newStateRoot != bytes32(0),
            "OnChainProposer: cannot verify an uncommitted batch"
        );

        // Construct public inputs from committed batch data
        bytes memory publicInputs = _getPublicInputsFromCommitment(batchNumber);

        if (PICOVERIFIER != DEV_MODE) {
            // If the verification fails, it will revert.
            IPicoVerifier(PICOVERIFIER).verifyPicoProof(
                picoRiscvVkey,
                publicInputs,
                picoProof
            );
        }

        if (R0VERIFIER != DEV_MODE) {
            // If the verification fails, it will revert.
            IRiscZeroVerifier(R0VERIFIER).verify(
                risc0BlockProof,
                risc0ImageId,
                sha256(publicInputs)
            );
        }

        if (SP1VERIFIER != DEV_MODE) {
            // If the verification fails, it will revert.
            // SP1 verifier expects a 144-byte public values array with 16-byte prefix
            bytes memory sp1PublicValues = new bytes(144);
            // First 16 bytes are empty (used for other purposes in SP1)
            for (uint i = 16; i < 144; i++) {
                sp1PublicValues[i] = publicInputs[i - 16];
            }
            
            ISP1Verifier(SP1VERIFIER).verifyProof(
                sp1ProgramVKey,
                sp1PublicValues,
                sp1ProofBytes
            );
        }

        lastVerifiedBatch = batchNumber;
        // The first 2 bytes are the number of deposits.
        uint16 deposits_amount = uint16(
            bytes2(
                batchCommitments[batchNumber].processedDepositLogsRollingHash
            )
        );
        if (deposits_amount > 0) {
            ICommonBridge(BRIDGE).removePendingDepositLogs(deposits_amount);
        }

        // Remove previous batch commitment as it is no longer needed.
        delete batchCommitments[batchNumber - 1];

        emit BatchVerified(lastVerifiedBatch);
    }

    /// @notice Constructs public inputs from committed batch data for proof verification.
    /// @dev This function retrieves the necessary data from batch commitments and formats it
    /// into a 128-byte array that serves as public inputs for all proving systems.
    /// @dev Public inputs structure (128 bytes total):
    /// - bytes 0-32: Initial state root (from the last verified batch)
    /// - bytes 32-64: Final state root (from the current batch)
    /// - bytes 64-96: Withdrawals merkle root (from the current batch)
    /// - bytes 96-128: Deposits log hash (from the current batch)
    /// @dev The initial state root uses the last verified batch's newStateRoot because
    /// batch verification is sequential and each batch depends on the previous one.
    /// @param batchNumber The batch number to retrieve public inputs for.
    /// @return publicInputs The 128-byte public inputs array for proof verification.
    function _getPublicInputsFromCommitment(
        uint256 batchNumber
    ) internal view returns (bytes memory) {
        BatchCommitmentInfo memory currentBatch = batchCommitments[batchNumber];
        BatchCommitmentInfo memory previousBatch = batchCommitments[lastVerifiedBatch];
        
        // Public inputs are 128 bytes:
        // - bytes 0-32: initial state root
        // - bytes 32-64: final state root
        // - bytes 64-96: withdrawals merkle root
        // - bytes 96-128: deposits log hash
        bytes memory publicInputs = new bytes(128);
        
        // Initial state root from the last verified batch
        bytes32 initialStateRoot = previousBatch.newStateRoot;
        for (uint i = 0; i < 32; i++) {
            publicInputs[i] = bytes1(uint8(uint256(initialStateRoot) >> (8 * (31 - i))));
        }
        
        // Final state root from the current batch
        bytes32 finalStateRoot = currentBatch.newStateRoot;
        for (uint i = 0; i < 32; i++) {
            publicInputs[32 + i] = bytes1(uint8(uint256(finalStateRoot) >> (8 * (31 - i))));
        }
        
        // Withdrawals merkle root
        bytes32 withdrawalsRoot = currentBatch.withdrawalsLogsMerkleRoot;
        for (uint i = 0; i < 32; i++) {
            publicInputs[64 + i] = bytes1(uint8(uint256(withdrawalsRoot) >> (8 * (31 - i))));
        }
        
        // Deposits log hash
        bytes32 depositsHash = currentBatch.processedDepositLogsRollingHash;
        for (uint i = 0; i < 32; i++) {
            publicInputs[96 + i] = bytes1(uint8(uint256(depositsHash) >> (8 * (31 - i))));
        }
        
        return publicInputs;
    }

    /// @notice Allow owner to upgrade the contract.
    /// @param newImplementation the address of the new implementation
    function _authorizeUpgrade(
        address newImplementation
    ) internal virtual override onlyOwner {}
}
