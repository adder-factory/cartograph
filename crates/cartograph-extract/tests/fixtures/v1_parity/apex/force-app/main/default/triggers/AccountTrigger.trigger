trigger AccountTrigger on Account (before insert, after update) {
    AccountService svc = new AccountService();
    for (Account acct : Trigger.new) {
        svc.load(acct.Name);
    }
    Formatter.format('done');
}
