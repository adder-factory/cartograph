public with sharing class AccountController {
    public Account account { get; set; }

    public AccountController() {
        account = new Account();
    }

    public PageReference save() {
        upsert account;
        return null;
    }

    public PageReference cancel() {
        return Page.AccountPage;
    }
}
