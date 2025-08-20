#!/usr/bin/env python3
"""
Script to merge multiple Swagger/OpenAPI 2.0 files into a single specification.
This merges service.swagger.json, indexer.swagger.json, and types.swagger.json
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

def merge_swagger_files():
    """Merge the three swagger files into one"""
    
    # Read the three swagger files
    with open('swagger/service.swagger.json', 'r') as f:
        service = json.load(f)
    
    with open('swagger/indexer.swagger.json', 'r') as f:
        indexer = json.load(f)
    
    with open('swagger/types.swagger.json', 'r') as f:
        types = json.load(f)
    
    # Start with service as base (it has the main API)
    merged = service.copy()
    
    # Update info section
    merged['info'] = {
        'title': 'Ark API',
        'version': '1.0.0',
        'description': 'Combined Ark Service and Indexer API'
    }
    
    # Merge tags
    if 'tags' not in merged:
        merged['tags'] = []
    
    # Add indexer tags
    for tag in indexer.get('tags', []):
        if tag not in merged['tags']:
            merged['tags'].append(tag)
    
    # Merge paths from indexer
    if 'paths' not in merged:
        merged['paths'] = {}
    merged['paths'] = deep_merge(merged['paths'], indexer.get('paths', {}))
    
    # Merge definitions from all three files
    if 'definitions' not in merged:
        merged['definitions'] = {}
    
    # First merge types definitions (base types)
    merged['definitions'] = deep_merge(merged['definitions'], types.get('definitions', {}))
    
    # Then merge indexer definitions
    merged['definitions'] = deep_merge(merged['definitions'], indexer.get('definitions', {}))
    
    # Service definitions are already in merged
    
    # Write the merged swagger file
    with open('swagger/merged.swagger.json', 'w') as f:
        json.dump(merged, f, indent=2)
    
    print("✅ Successfully merged swagger files into swagger/merged.swagger.json")
    
    # Also create an OpenAPI 3.0 version if needed
    create_openapi3_version(merged)

def create_openapi3_version(swagger2: Dict[str, Any]):
    """Convert Swagger 2.0 to OpenAPI 3.0 format (basic conversion)"""
    
    openapi3 = {
        'openapi': '3.0.0',
        'info': swagger2.get('info', {}),
        'servers': [
            {
                'url': 'http://localhost:8080',
                'description': 'Local server'
            }
        ],
        'paths': {},
        'components': {
            'schemas': {}
        }
    }
    
    # Convert paths
    for path, methods in swagger2.get('paths', {}).items():
        openapi3['paths'][path] = {}
        for method, operation in methods.items():
            if method in ['get', 'post', 'put', 'delete', 'patch', 'options', 'head']:
                converted_op = convert_operation(operation)
                openapi3['paths'][path][method] = converted_op
    
    # Convert definitions to components/schemas
    for name, schema in swagger2.get('definitions', {}).items():
        openapi3['components']['schemas'][name] = convert_schema(schema)
    
    # Write the OpenAPI 3.0 file
    with open('swagger/merged.openapi3.json', 'w') as f:
        json.dump(openapi3, f, indent=2)
    
    print("✅ Also created OpenAPI 3.0 version at swagger/merged.openapi3.json")

def convert_operation(operation: Dict[str, Any]) -> Dict[str, Any]:
    """Convert a Swagger 2.0 operation to OpenAPI 3.0"""
    converted = {
        'summary': operation.get('summary', ''),
        'description': operation.get('description', ''),
        'operationId': operation.get('operationId', ''),
        'tags': operation.get('tags', []),
        'responses': {}
    }
    
    # Convert parameters
    if 'parameters' in operation:
        converted['parameters'] = []
        body_param = None
        
        for param in operation['parameters']:
            if param.get('in') == 'body':
                body_param = param
            else:
                converted['parameters'].append(param)
        
        # Convert body parameter to requestBody
        if body_param:
            converted['requestBody'] = {
                'required': body_param.get('required', False),
                'content': {
                    'application/json': {
                        'schema': body_param.get('schema', {})
                    }
                }
            }
    
    # Convert responses
    for status, response in operation.get('responses', {}).items():
        converted['responses'][status] = convert_response(response)
    
    return converted

def convert_response(response: Dict[str, Any]) -> Dict[str, Any]:
    """Convert a Swagger 2.0 response to OpenAPI 3.0"""
    converted = {
        'description': response.get('description', '')
    }
    
    if 'schema' in response:
        converted['content'] = {
            'application/json': {
                'schema': response['schema']
            }
        }
    
    return converted

def convert_schema(schema: Dict[str, Any]) -> Dict[str, Any]:
    """Convert a Swagger 2.0 schema to OpenAPI 3.0"""
    # For basic conversion, most of the schema stays the same
    # Just need to handle $ref changes
    converted = {}
    for key, value in schema.items():
        if key == '$ref' and value.startswith('#/definitions/'):
            converted['$ref'] = value.replace('#/definitions/', '#/components/schemas/')
        elif isinstance(value, dict):
            converted[key] = convert_schema(value)
        elif isinstance(value, list):
            converted[key] = [convert_schema(item) if isinstance(item, dict) else item for item in value]
        else:
            converted[key] = value
    return converted

if __name__ == '__main__':
    try:
        merge_swagger_files()
    except Exception as e:
        print(f"❌ Error merging swagger files: {e}", file=sys.stderr)
        sys.exit(1)