"""CPU contracts for GI evidence tools; no renderer mutation or desktop input."""

import sys
import unittest

sys.dont_write_bytecode = True

import numpy as np

from gi_scene_regression import display_srgb8


class DisplayEncodingTests(unittest.TestCase):
    def test_linear_unorm_display_anchors(self):
        source = np.array([0, 1, 128, 255], dtype=np.uint8)
        np.testing.assert_array_equal(display_srgb8(source), [0, 13, 188, 255])
        np.testing.assert_array_equal(source, [0, 1, 128, 255])

    def test_shape_and_monotonicity(self):
        source = np.arange(256, dtype=np.uint8).reshape(2, 4, 8, 4)
        encoded = display_srgb8(source)
        self.assertEqual(encoded.shape, source.shape)
        self.assertEqual(encoded.dtype, np.uint8)
        self.assertTrue(np.all(np.diff(encoded.reshape(-1).astype(np.int32)) >= 0))

    def test_display_encoding_is_not_exposure_gain(self):
        encoded = display_srgb8(np.array([0, 255], dtype=np.uint8))
        np.testing.assert_array_equal(encoded, [0, 255])
        self.assertGreater(display_srgb8(np.array(8, dtype=np.uint8)).item(), 8)


class PixelEnergyTests(unittest.TestCase):
    def test_coefficients_are_evaluated_at_the_requested_normal(self):
        from gi_pixel_audit import evaluate
        coefficients = {"l0": [100, 100, 100], "l1x": [100, 0, -100],
                        "l1y": [0, 0, 0], "l1z": [0, 0, 0]}
        np.testing.assert_array_equal(evaluate(coefficients, [1, 0, 0]), [200, 100, 0])
        np.testing.assert_array_equal(evaluate(coefficients, [-1, 0, 0]), [0, 100, 200])


if __name__ == "__main__":
    unittest.main()
