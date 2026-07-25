#!/usr/bin/env python3

import unittest

from publish_order import publish_order


class PublishOrderTests(unittest.TestCase):
    def test_orders_transitive_workspace_dependencies_before_root(self) -> None:
        metadata = {
            "workspace_members": ["yolop-id", "sdk-id", "inner-id"],
            "packages": [
                {
                    "id": "yolop-id",
                    "name": "yolop",
                    "publish": None,
                    "dependencies": [{"name": "yolop-yep", "path": "sdk"}],
                },
                {
                    "id": "sdk-id",
                    "name": "yolop-yep",
                    "publish": None,
                    "dependencies": [{"name": "yolop-yep-inner", "path": "inner"}],
                },
                {
                    "id": "inner-id",
                    "name": "yolop-yep-inner",
                    "publish": None,
                    "dependencies": [],
                },
            ],
        }

        self.assertEqual(
            publish_order(metadata),
            ["yolop-yep-inner", "yolop-yep", "yolop"],
        )

    def test_excludes_non_publishable_dependency(self) -> None:
        metadata = {
            "workspace_members": ["yolop-id", "private-id"],
            "packages": [
                {
                    "id": "yolop-id",
                    "name": "yolop",
                    "publish": None,
                    "dependencies": [{"name": "private", "path": "private"}],
                },
                {
                    "id": "private-id",
                    "name": "private",
                    "publish": [],
                    "dependencies": [],
                },
            ],
        }

        self.assertEqual(publish_order(metadata), ["yolop"])


if __name__ == "__main__":
    unittest.main()
