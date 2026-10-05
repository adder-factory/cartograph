import { LightningElement, wire } from 'lwc';
import listAccounts from '@salesforce/apex/AccountService.listAccounts';
import contactsFor from '@salesforce/apex/AccountService.contactsFor';

export default class AccountList extends LightningElement {
    @wire(listAccounts) accounts;

    connectedCallback() {
        contactsFor({ accountId: 'x' });
    }
}
