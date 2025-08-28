// Script to generate test vectors from TypeScript SDK for comparison
// Run this in the arkade-os/ts-sdk repository to get the expected hex values

const { VhtlcScript } = require('../path/to/ts-sdk'); // Adjust path as needed

// Test data from fixtures (same as used in Rust tests)
const testData = {
  preimageHash: Buffer.from('4d487dd3753a89bc9fe98401d1196523058251fc', 'hex'),
  receiver: Buffer.from('021e1bb85455fe3f5aed60d101aa4dbdb9e7714f6226769a97a17a5331dadcd53b', 'hex'),
  sender: Buffer.from('030192e796452d6df9697c280542e1560557bcf79a347d925895043136225c7cb4', 'hex'),
  server: Buffer.from('03aad52d58162e9eefeafc7ad8a1cdca8060b5f01df1e7583362d052e266208f88', 'hex'),
  refundLocktime: 265,
  unilateralClaimDelay: { type: 'blocks', value: 17 },
  unilateralRefundDelay: { type: 'blocks', value: 144 },
  unilateralRefundWithoutReceiverDelay: { type: 'blocks', value: 144 }
};

try {
  // Create VHTLC instance
  const vhtlc = new VhtlcScript(testData);
  
  console.log('=== TypeScript SDK VHTLC Script Vectors ===\n');
  
  // Export all script hex values
  const vectors = {
    claim: vhtlc.claim().script.toString('hex'),
    refund: vhtlc.refund().script.toString('hex'),
    refundWithoutReceiver: vhtlc.refundWithoutReceiver().script.toString('hex'),
    unilateralClaim: vhtlc.unilateralClaim().script.toString('hex'),
    unilateralRefund: vhtlc.unilateralRefund().script.toString('hex'),
    unilateralRefundWithoutReceiver: vhtlc.unilateralRefundWithoutReceiver().script.toString('hex')
  };
  
  // Output for comparison
  console.log('TypeScript SDK Script Vectors:');
  console.log('1. Claim Script:                          ', vectors.claim);
  console.log('2. Refund Script:                         ', vectors.refund);
  console.log('3. Refund Without Receiver Script:        ', vectors.refundWithoutReceiver);
  console.log('4. Unilateral Claim Script:               ', vectors.unilateralClaim);
  console.log('5. Unilateral Refund Script:              ', vectors.unilateralRefund);
  console.log('6. Unilateral Refund Without Receiver:    ', vectors.unilateralRefundWithoutReceiver);
  
  // Also output as JSON for easy parsing
  console.log('\n=== JSON Format ===');
  console.log(JSON.stringify(vectors, null, 2));
  
  // Export taproot address for comparison
  const address = vhtlc.address('testnet'); // or 'mainnet'
  console.log('\n=== Address ===');
  console.log('TypeScript SDK Address:', address);
  
} catch (error) {
  console.error('Error generating TypeScript vectors:', error);
  console.log('\nPlease ensure:');
  console.log('1. You are running this in the arkade-os/ts-sdk directory');
  console.log('2. The path to VhtlcScript is correct');
  console.log('3. All dependencies are installed (npm install)');
}