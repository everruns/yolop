#!/usr/bin/env python3

import unittest

from publish_order import publish_order


class PublishOrderTests(unittest.TestCase):
    def test_orders_transitive_workspace_dependencies_before_root(self) -> None:
        metadata = {
            "workspace_members": ["yolop-id", "tuika-id", "formatter-id", "sdk-id"],
            "packages": [
                {
                    "id": "yolop-id",
                    "name": "yolop",
                    "publish": None,
                    "dependencies": [
                        {"name": "tuika-codeformatters", "path": "formatter"},
                        {"name": "yolop-yep", "path": "sdk"},
                    ],
                },
                {
                    "id": "formatter-id",
                    "name": "tuika-codeformatters",
                    "publish": None,
                    "dependencies": [{"name": "tuika", "path": "tuika"}],
                },
                {
                    "id": "tuika-id",
                    "name": "tuika",
                    "publish": None,
                    "dependencies": [],
                },
                {
                    "id": "sdk-id",
                    "name": "yolop-yep",
                    "publish": None,
                    "dependencies": [],
                },
            ],
        }

        self.assertEqual(
            publish_order(metadata),
            ["tuika", "tuika-codeformatters", "yolop-yep", "yolop"],
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
