import {test} from 'node:test';
import assert from 'node:assert/strict';
import {mkdtempSync,readFileSync,writeFileSync,rmSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {pathToFileURL} from 'node:url';
// Exercise the maintained view code; only resolve the SDK import to the local core.
const temp=mkdtempSync(join(tmpdir(),'wallet-view-'));
const views=[];
try {
  for (const [name,file] of [['Vite','examples/browser-wallet/src/wallet-view.ts'],['Next','examples/next-wallet/lib/wallet-view.ts']]) {
    const copy=join(temp,name+'.ts');
    const source=readFileSync(new URL('../'+file,import.meta.url),'utf8');
    writeFileSync(copy,source.replace('"@z-stack/sdk"',JSON.stringify(new URL('../packages/core/src/index.ts',import.meta.url).href)));
    views.push([name,await import(pathToFileURL(copy).href)]);
  }
} finally {rmSync(temp,{recursive:true,force:true});}

const mined = {txid:'a'.repeat(64), status:'mined', minedHeight:4482837, expiryHeight:null,
  accountDeltaZat:12500000, spentZat:0, receivedZat:12500000, feeZat:null,
  sentNoteCount:0, receivedNoteCount:1, memoCount:0, hasChange:false,
  isShielding:false, expiredUnmined:false};
for (const [name, view] of views) {
  test(`${name}: a public receive remains confirming until the configured threshold`, () => {
    const snapshot = {birthdayHeight:4482700, scannedHeight:4482837,
      confirmations:{trusted:1,untrusted:3,zeroConfShield:false},
      balance:{totalAvailable:0,totalPending:12500000,pendingZec:'0.12500000'}};
    assert.equal(view.pendingFunds(snapshot),12500000n);
    assert.equal(view.confirmationLabel(mined,snapshot),'Confirming · 1/3 confirmations');
    snapshot.scannedHeight++;
    assert.equal(view.confirmationLabel(mined,snapshot),'Confirming · 2/3 confirmations');
    snapshot.scannedHeight++;
    assert.equal(view.confirmationLabel(mined,snapshot),'Mined · 3 confirmations');
    assert.equal(view.confirmationLabel({...mined,confirmations:2},snapshot),'Confirming · 2/3 confirmations');
    // Counts describe history, never override the authoritative balance.
    assert.equal(view.pendingFunds({...snapshot,balance:{totalPending:0,pendingZec:'0.12500000'}}),0n);
    assert.equal(view.pendingFunds({...snapshot,balance:{pendingZec:'0.12500000'}}),12500000n);
  });
  test(`${name}: unknown, unmined and expired activity does not invent confirmations`, () => {
    const snapshot={birthdayHeight:100,balance:{},confirmations:{trusted:1,untrusted:3}};
    assert.equal(view.confirmationLabel(mined,snapshot),'Mined · confirmation count unavailable');
    assert.equal(view.confirmationLabel({...mined,status:'pending',minedHeight:null},snapshot),'Pending');
    assert.equal(view.confirmationLabel({...mined,status:'expired'},snapshot),'Expired');
    assert.equal(view.confirmationLabel({...mined,confirmations:1,accountDeltaZat:-12500000,receivedZat:0,spentZat:12500000,sentNoteCount:1},snapshot),'Mined · 1 confirmation');
    assert.equal(view.loadedWalletStatus(snapshot),'Wallet opened · not scanned yet. Sync to find activity.');
    assert.match(view.loadedWalletStatus({...snapshot,scannedHeight:105}),/scanned through block 105/);
  });
}

for (const [name, view] of views) {
  test(`${name}: compact self-send movement does not pretend to know its payment amount`, () => {
    const compact = {...mined,accountDeltaZat:-10000,spentZat:12500000,receivedZat:12490000,
      orchardSpent:12500000,orchardReceived:12490000,sentNoteCount:1,feeZat:null,historyMetadataComplete:false};
    assert.equal(view.activityMovement(compact,'TAZ'),'Wallet change · −0.00010000 TAZ · load details for the payment amount');
    assert.equal(view.activityMovement({...compact,historyMetadataComplete:true,feeZat:10000},'TAZ'),'Self send · Fee 0.00010000 TAZ');
    assert.match(view.activityMovement(mined,'TAZ'),/Received · \+0.12500000 TAZ/);
  });
  test(`${name}: insufficient funds explains incoming and change thresholds without overriding balances`, () => {
    const snapshot={birthdayHeight:100,balance:{totalPending:12500000},confirmations:{trusted:1,untrusted:3}};
    assert.match(view.paymentErrorMessage(new Error('insufficient funds'),snapshot,'TAZ'),/0.12500000 TAZ is still confirming\. Incoming payments need 3 confirmations; your change needs 1 confirmation/);
    assert.equal(view.paymentErrorMessage(new Error('invalid recovery phrase: secret words'),snapshot,'TAZ'),'Those words are not a valid recovery phrase.');
    assert.match(view.paymentErrorMessage(new Error('insufficient funds'),{...snapshot,balance:{totalPending:0}},'TAZ'),/Not enough shielded funds/);
    assert.doesNotMatch(view.activityMovement({...mined,spentZat:10000,receivedZat:0,accountDeltaZat:-10000},'TAZ'),/load details/);
    assert.equal(view.confirmationPolicyText({trusted:1,untrusted:1}),'Funds need 1 confirmation. Sync to update.');
    const ownChange = {...snapshot,balance:{totalPending:40000}};
    assert.match(view.paymentErrorMessage(new Error('insufficient funds'),ownChange,'TAZ'),/your change needs 1 confirmation/);
    assert.equal(view.confirmationPolicyText(undefined),'Sync to update.');
    assert.match(view.syncedWalletStatus({...snapshot,scannedHeight:101}),/block 101.*payments mined later/);
  });
}

for (const [name, view] of views) {
  test(`${name}: a complete visible page cannot hide remaining memo batches`, () => {
    const visible = Array.from({length:20}, () => ({...mined,historyMetadataComplete:true}));
    assert.match(view.memoDetailsMessage({memoFetchStatus:"scanning"},visible),/Load again for remaining/);
    assert.equal(view.memoDetailsMessage({memoFetchStatus:"complete"},visible),"Memos and transaction details loaded.");
    assert.match(view.memoDetailsMessage({memoFetchStatus:"unavailable"},visible),/Try again later/);
    assert.match(view.memoDetailsMessage({},visible),/check remaining details/);
    assert.match(view.memoDetailsMessage({memoFetchStatus:"unsupported"},visible),/cannot load/);
  });
}
