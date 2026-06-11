// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test} from "forge-std/Test.sol";
import {PixelCanvas} from "../src/PixelCanvas.sol";

contract PixelCanvasTest is Test {
    PixelCanvas canvas;
    address alice = address(0xA11CE);
    address bob = address(0xB0B);

    event PixelsSet(address indexed artist, uint16[] positions, uint8[] colors);

    function setUp() public {
        canvas = new PixelCanvas();
    }

    // ── helpers ─────────────────────────────────────────────────────────────
    function _one(uint16 pos, uint8 color)
        internal
        pure
        returns (uint16[] memory p, uint8[] memory c)
    {
        p = new uint16[](1);
        c = new uint8[](1);
        p[0] = pos;
        c[0] = color;
    }

    // ── basic painting ──────────────────────────────────────────────────────
    function test_setSinglePixel() public {
        (uint16[] memory p, uint8[] memory c) = _one(0, 7);
        vm.prank(alice);
        canvas.setPixels(p, c);

        assertEq(canvas.pixel(0), 7);
        assertEq(canvas.totalPixels(), 1);
        assertEq(canvas.pixelsBy(alice), 1);
    }

    function test_overwritePixel() public {
        (uint16[] memory p, uint8[] memory c) = _one(100, 3);
        canvas.setPixels(p, c);
        (p, c) = _one(100, 12);
        canvas.setPixels(p, c);
        assertEq(canvas.pixel(100), 12);
        assertEq(canvas.totalPixels(), 2); // overwrites count
    }

    function test_neighborsWithinWordUntouched() public {
        // pos 33 and 34 share word 1 with pos 32..63; painting 33 must not disturb 32/34.
        (uint16[] memory p, uint8[] memory c) = _one(32, 5);
        canvas.setPixels(p, c);
        (p, c) = _one(34, 9);
        canvas.setPixels(p, c);
        (p, c) = _one(33, 15);
        canvas.setPixels(p, c);

        assertEq(canvas.pixel(32), 5);
        assertEq(canvas.pixel(33), 15);
        assertEq(canvas.pixel(34), 9);
    }

    function test_batchAcrossWordBoundary() public {
        // 60..67 spans word 1 (32..63) and word 2 (64..95).
        uint16[] memory p = new uint16[](8);
        uint8[] memory c = new uint8[](8);
        for (uint16 i = 0; i < 8; i++) {
            p[i] = 60 + i;
            c[i] = uint8(i + 1);
        }
        canvas.setPixels(p, c);
        for (uint16 i = 0; i < 8; i++) {
            assertEq(canvas.pixel(60 + i), uint8(i + 1));
        }
    }

    function test_duplicatePositionInBatch_lastWins() public {
        uint16[] memory p = new uint16[](2);
        uint8[] memory c = new uint8[](2);
        p[0] = 500;
        c[0] = 2;
        p[1] = 500;
        c[1] = 11;
        canvas.setPixels(p, c);
        assertEq(canvas.pixel(500), 11);
    }

    function test_lastAndFirstPixel() public {
        (uint16[] memory p, uint8[] memory c) = _one(4095, 15);
        canvas.setPixels(p, c);
        assertEq(canvas.pixel(4095), 15);
        // first pixel of last word (4064) untouched
        assertEq(canvas.pixel(4064), 0);
    }

    function test_emitsEvent() public {
        (uint16[] memory p, uint8[] memory c) = _one(42, 4);
        vm.expectEmit(true, false, false, true);
        emit PixelsSet(alice, p, c);
        vm.prank(alice);
        canvas.setPixels(p, c);
    }

    // ── getCanvas linearization ─────────────────────────────────────────────
    function test_getCanvasMatchesPixelReads() public {
        uint16[] memory p = new uint16[](5);
        uint8[] memory c = new uint8[](5);
        p[0] = 0;
        p[1] = 31; // end of word 0
        p[2] = 32; // start of word 1
        p[3] = 2048; // word 64
        p[4] = 4095; // last byte of last word
        c[0] = 1;
        c[1] = 2;
        c[2] = 3;
        c[3] = 4;
        c[4] = 5;
        canvas.setPixels(p, c);

        bytes memory out = canvas.getCanvas();
        assertEq(out.length, 4096);
        for (uint256 k = 0; k < 5; k++) {
            assertEq(uint8(out[p[k]]), c[k]);
        }
        // spot-check an untouched byte
        assertEq(uint8(out[1000]), 0);
    }

    function testFuzz_packing(uint16 pos, uint8 color) public {
        pos = uint16(bound(pos, 0, 4095));
        color = uint8(bound(color, 0, 15));
        (uint16[] memory p, uint8[] memory c) = _one(pos, color);
        canvas.setPixels(p, c);
        assertEq(canvas.pixel(pos), color);
        bytes memory out = canvas.getCanvas();
        assertEq(uint8(out[pos]), color);
    }

    // ── validation ──────────────────────────────────────────────────────────
    function test_revert_posOutOfRange() public {
        (uint16[] memory p, uint8[] memory c) = _one(4096, 1);
        vm.expectRevert(bytes("pos out of range"));
        canvas.setPixels(p, c);
    }

    function test_revert_colorOutOfRange() public {
        (uint16[] memory p, uint8[] memory c) = _one(0, 16);
        vm.expectRevert(bytes("color out of range"));
        canvas.setPixels(p, c);
    }

    function test_revert_lengthMismatch() public {
        uint16[] memory p = new uint16[](2);
        uint8[] memory c = new uint8[](1);
        vm.expectRevert(bytes("length mismatch"));
        canvas.setPixels(p, c);
    }

    function test_revert_emptyBatch() public {
        uint16[] memory p = new uint16[](0);
        uint8[] memory c = new uint8[](0);
        vm.expectRevert(bytes("batch size"));
        canvas.setPixels(p, c);
    }

    function test_revert_batchTooLarge() public {
        uint16[] memory p = new uint16[](129);
        uint8[] memory c = new uint8[](129);
        vm.expectRevert(bytes("batch size"));
        canvas.setPixels(p, c);
    }

    function test_maxBatchAllowed() public {
        uint16[] memory p = new uint16[](128);
        uint8[] memory c = new uint8[](128);
        for (uint16 i = 0; i < 128; i++) {
            p[i] = i;
            c[i] = uint8(i % 16);
        }
        canvas.setPixels(p, c);
        assertEq(canvas.totalPixels(), 128);
    }

    // ── block pixel cap ─────────────────────────────────────────────────────
    function test_blockPixelCap() public {
        uint16[] memory p = new uint16[](128);
        uint8[] memory c = new uint8[](128);
        for (uint16 i = 0; i < 128; i++) {
            p[i] = i;
            c[i] = 1;
        }
        // 8 * 128 = 1024 fills the per-block cap exactly
        for (uint256 r = 0; r < 8; r++) {
            canvas.setPixels(p, c);
        }
        assertEq(canvas.paintedInBlock(block.number), 1024);

        (uint16[] memory p1, uint8[] memory c1) = _one(0, 1);
        vm.expectRevert(bytes("block pixel cap"));
        canvas.setPixels(p1, c1);

        // next block: painting works again
        vm.roll(block.number + 1);
        canvas.setPixels(p1, c1);
        assertEq(canvas.paintedInBlock(block.number), 1);
    }

    // ── pause / admin ───────────────────────────────────────────────────────
    function test_pauseBlocksPainting() public {
        canvas.setPaused(true);
        (uint16[] memory p, uint8[] memory c) = _one(0, 1);
        vm.prank(alice);
        vm.expectRevert(bytes("paused"));
        canvas.setPixels(p, c);

        canvas.setPaused(false);
        vm.prank(alice);
        canvas.setPixels(p, c);
        assertEq(canvas.pixel(0), 1);
    }

    function test_onlyOwnerCanPause() public {
        vm.prank(bob);
        vm.expectRevert(bytes("not owner"));
        canvas.setPaused(true);
    }

    function test_pixelRead_revertOutOfRange() public {
        vm.expectRevert(bytes("pos out of range"));
        canvas.pixel(4096);
    }

    // ── per-artist accounting ───────────────────────────────────────────────
    function test_perArtistCounts() public {
        uint16[] memory p = new uint16[](3);
        uint8[] memory c = new uint8[](3);
        p[0] = 1;
        p[1] = 2;
        p[2] = 3;
        c[0] = 1;
        c[1] = 2;
        c[2] = 3;
        vm.prank(alice);
        canvas.setPixels(p, c);
        (uint16[] memory p1, uint8[] memory c1) = _one(9, 9);
        vm.prank(bob);
        canvas.setPixels(p1, c1);

        assertEq(canvas.pixelsBy(alice), 3);
        assertEq(canvas.pixelsBy(bob), 1);
        assertEq(canvas.totalPixels(), 4);
    }
}
