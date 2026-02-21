import 'dart:ffi';
import 'package:flutter/material.dart';

// Load the native library. In a real application the path would depend on the
// platform (Android/iOS/macOS). For simplicity this uses `DynamicLibrary.process`.
final DynamicLibrary native = DynamicLibrary.process();

typedef _InitClientNative = Void Function();

typedef _GetOffchainAddressNative = Pointer<Utf8> Function();

final _initClient = native.lookupFunction<_InitClientNative, void Function()>('init_client');
final _getOffchainAddress = native
    .lookupFunction<_GetOffchainAddressNative, Pointer<Utf8> Function()>('get_offchain_address');

void main() {
  runApp(const MyApp());
}

class MyApp extends StatefulWidget {
  const MyApp({super.key});

  @override
  State<MyApp> createState() => _MyAppState();
}

class _MyAppState extends State<MyApp> {
  String address = '';

  @override
  void initState() {
    super.initState();
    _initClient();
    final ptr = _getOffchainAddress();
    address = ptr.cast<Utf8>().toDartString();
  }

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      home: Scaffold(
        appBar: AppBar(title: const Text('Ark Flutter Example')),
        body: Center(
          child: Text(address.isEmpty ? 'Loading...' : address),
        ),
      ),
    );
  }
}
