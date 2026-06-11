// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @title PixelCanvas — a 64x64 shared pixel canvas with a 16-color palette.
/// @notice Quest step 4 target contract for the Koinos EVM demo chain.
///
///         Grid: 64x64 = 4096 pixels. Position encoding: pos = y*64 + x
///         (uint16, valid range 0..4095). Colors are uint8 palette INDICES in
///         0..15 — the palette itself lives client-side.
///
///         Storage is packed 32 pixels per 256-bit word (8 bits per pixel),
///         128 words total. Word i holds positions [i*32 .. i*32+31]; pixel p
///         lives in word p/32 at bits (p%32)*8 .. (p%32)*8+7 (little-end-first
///         within the word). `getCanvas()` linearizes this back so that
///         byte[p] == color at position p.
contract PixelCanvas {
    // ── constants ───────────────────────────────────────────────────────────
    uint256 public constant WIDTH = 64;
    uint256 public constant PIXELS = 4096; // WIDTH * WIDTH
    uint256 public constant WORDS = 128; // PIXELS / 32
    uint256 public constant MAX_BATCH = 128; // max pixels per setPixels() call
    uint256 public constant BLOCK_PIXEL_CAP = 1024; // max pixels painted per block

    // ── state ───────────────────────────────────────────────────────────────
    /// @dev Packed canvas: 32 pixels (8 bits each) per word, 128 words.
    uint256[WORDS] private _words;

    /// @notice Total pixels painted (overwrites count too).
    uint256 public totalPixels;

    /// @notice Pixels painted per artist.
    mapping(address => uint256) public pixelsBy;

    /// @notice Pixels painted in a given block (global anti-spam cap).
    mapping(uint256 => uint256) public paintedInBlock;

    /// @notice Deployer; may pause/unpause painting.
    address public owner;

    /// @notice When true, setPixels() reverts.
    bool public paused;

    // ── events ──────────────────────────────────────────────────────────────
    /// @notice Emitted once per setPixels() batch.
    ///         topic0 = keccak256("PixelsSet(address,uint16[],uint8[])")
    event PixelsSet(address indexed artist, uint16[] positions, uint8[] colors);

    // ── auth ────────────────────────────────────────────────────────────────
    modifier onlyOwner() {
        require(msg.sender == owner, "not owner");
        _;
    }

    constructor() {
        owner = msg.sender;
    }

    // ── painting ────────────────────────────────────────────────────────────
    /// @notice Paint a batch of pixels (1..128 per call, 1024 per block globally).
    /// @param positions pixel positions, each < 4096 (pos = y*64 + x)
    /// @param colors    palette indices, each < 16, paired with positions
    function setPixels(uint16[] calldata positions, uint8[] calldata colors) external {
        require(!paused, "paused");
        uint256 n = positions.length;
        require(n == colors.length, "length mismatch");
        require(n >= 1 && n <= MAX_BATCH, "batch size");

        uint256 painted = paintedInBlock[block.number] + n;
        require(painted <= BLOCK_PIXEL_CAP, "block pixel cap");
        paintedInBlock[block.number] = painted;

        // Word cache: consecutive positions share a storage word 32 at a time,
        // so drag strokes touch far fewer SLOAD/SSTOREs than scattered dots.
        uint256 curIdx = type(uint256).max; // sentinel: no word loaded
        uint256 curWord;

        for (uint256 k = 0; k < n; ++k) {
            uint256 pos = positions[k];
            uint256 color = colors[k];
            require(pos < PIXELS, "pos out of range");
            require(color < 16, "color out of range");

            uint256 wordIdx = pos >> 5; // pos / 32
            uint256 shift = (pos & 31) << 3; // (pos % 32) * 8

            if (wordIdx != curIdx) {
                if (curIdx != type(uint256).max) {
                    _words[curIdx] = curWord;
                }
                curIdx = wordIdx;
                curWord = _words[wordIdx];
            }
            curWord = (curWord & ~(uint256(0xff) << shift)) | (color << shift);
        }
        _words[curIdx] = curWord; // n >= 1, so a word is always loaded

        totalPixels += n;
        pixelsBy[msg.sender] += n;
        emit PixelsSet(msg.sender, positions, colors);
    }

    // ── reads ───────────────────────────────────────────────────────────────
    /// @notice Full canvas as exactly 4096 bytes; byte[p] = color at position p.
    function getCanvas() external view returns (bytes memory out) {
        out = new bytes(PIXELS);
        for (uint256 i = 0; i < WORDS; ++i) {
            uint256 word = _words[i];
            if (word == 0) continue; // untouched word: bytes are already zero
            uint256 base = i << 5; // i * 32
            for (uint256 j = 0; j < 32; ++j) {
                out[base + j] = bytes1(uint8(word >> (j << 3)));
            }
        }
    }

    /// @notice Color of a single pixel (convenience read).
    function pixel(uint16 pos) external view returns (uint8) {
        require(pos < PIXELS, "pos out of range");
        return uint8(_words[pos >> 5] >> ((pos & 31) << 3));
    }

    // ── admin ───────────────────────────────────────────────────────────────
    function setPaused(bool p) external onlyOwner {
        paused = p;
    }
}
