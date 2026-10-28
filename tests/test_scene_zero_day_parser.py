import unittest

import scene_zero_day as scene


class NodeTransformTests(unittest.TestCase):
    def test_parent_translation_composes_with_child_trs(self):
        nodes = [
            {"translation": [10.0, 0.0, 0.0], "children": [1]},
            {
                "translation": [0.0, 3.0, 0.0],
                "rotation": [0.0, 0.0, 2**-0.5, 2**-0.5],
                "scale": [2.0, 1.0, 1.0],
            },
        ]
        world = scene._node_world_matrices(nodes)
        point = scene._xform(world[1], (1.0, 0.0, 0.0))
        for actual, expected in zip(point, (10.0, 5.0, 0.0)):
            self.assertAlmostEqual(actual, expected, places=5)

    def test_matrix_takes_precedence_over_trs(self):
        matrix = [
            1.0, 0.0, 0.0, 0.0,
            0.0, 1.0, 0.0, 0.0,
            0.0, 0.0, 1.0, 0.0,
            4.0, 5.0, 6.0, 1.0,
        ]
        actual = scene._node_local_matrix({"matrix": matrix, "translation": [99, 99, 99]})
        self.assertEqual(actual, matrix)


if __name__ == "__main__":
    unittest.main()
