#!/usr/bin/env python3
"""
Script to merge multiple OpenAPI 3.0 files into a single specification.
This merges admin.openapi.json, indexer.openapi.json, service.openapi.json,
signer_manager.openapi.json, types.openapi.json, and wallet.openapi.json
"""

import json
import sys
from typing import Dict, Any

def deep_merge(base: Dict[str, Any], update: Dict[str, Any]) -> Dict[str, Any]:
    """Deep merge two dictionaries"""
    for key, value in update.items():
        if key in base:
            if isinstance(base[key], dict) and isinstance(value, dict):
                base[key] = deep_merge(base[key], value)
            elif isinstance(base[key], list) and isinstance(value, list):
                # For arrays, extend without duplicates
                for item in value:
                    if item not in base[key]:
                        base[key].append(item)
            else:
                # For other types, update overwrites base
                base[key] = value
        else:
            base[key] = value
    return base

def merge_openapi_files():
    """Merge the OpenAPI files into one"""

    # List of OpenAPI files to merge
    openapi_files = [
        'swagger/types.openapi.json',        # Base types first
        'swagger/service.openapi.json',      # Main service API
        'swagger/indexer.openapi.json',      # Indexer API
        'swagger/admin.openapi.json',        # Admin API
        'swagger/signer_manager.openapi.json', # Signer manager API
        'swagger/wallet.openapi.json',       # Wallet API
    ]

    # Read and merge all files
    merged = None

    for file_path in openapi_files:
        try:
            with open(file_path, 'r') as f:
                spec = json.load(f)

            if merged is None:
                # First file - use as base
                merged = spec.copy()
                # Update info section for merged spec
                merged['info'] = {
                    'title': 'Ark API',
                    'version': '1.0.0',
                    'description': 'Combined Ark Service, Indexer, Admin, Signer Manager, and Wallet API'
                }
            else:
                # Merge subsequent files
                # Merge tags
                if 'tags' in spec:
                    if 'tags' not in merged:
                        merged['tags'] = []
                    for tag in spec['tags']:
                        if tag not in merged['tags']:
                            merged['tags'].append(tag)

                # Merge paths
                if 'paths' in spec:
                    if 'paths' not in merged:
                        merged['paths'] = {}
                    merged['paths'] = deep_merge(merged['paths'], spec['paths'])

                # Merge components/schemas
                if 'components' in spec and 'schemas' in spec['components']:
                    if 'components' not in merged:
                        merged['components'] = {}
                    if 'schemas' not in merged['components']:
                        merged['components']['schemas'] = {}
                    merged['components']['schemas'] = deep_merge(
                        merged['components']['schemas'],
                        spec['components']['schemas']
                    )

                # Merge servers if needed
                if 'servers' in spec:
                    if 'servers' not in merged:
                        merged['servers'] = []
                    for server in spec['servers']:
                        if server not in merged['servers']:
                            merged['servers'].append(server)

        except FileNotFoundError:
            print(f"⚠️  Warning: {file_path} not found, skipping...")
            continue

    # Write the merged OpenAPI file
    with open('swagger/merged.openapi.json', 'w') as f:
        json.dump(merged, f, indent=2)

    print("✅ Successfully merged OpenAPI files into swagger/merged.openapi.json")


if __name__ == '__main__':
    try:
        merge_openapi_files()
    except Exception as e:
        print(f"❌ Error merging OpenAPI files: {e}", file=sys.stderr)
        sys.exit(1)