public with sharing class AccountService extends BaseService implements Queueable, Database.Batchable<SObject> {
    public enum Mode { FAST, SLOW }

    public static final Integer LIMIT_ROWS = 50;
    private Integer a, b;
    public String label { get; set; }
    private Mode mode;

    public interface Notifier {
        void notify(Account acct);
    }

    public class Result {
        public Integer count;
        public Result(Integer count) { this.count = count; }
    }

    public AccountService() {
        this.mode = Mode.FAST;
    }

    @AuraEnabled(cacheable=true)
    public static List<Account> listAccounts() {
        return [SELECT Id, Name FROM Account LIMIT 50];
    }

    @AuraEnabled
    public static List<Contact> contactsFor(String accountId) {
        List<Contact> rows = [SELECT Id FROM Contact WHERE AccountId = :accountId];
        return rows;
    }

    public List<List<SObject>> search(String term) {
        return [FIND :term IN ALL FIELDS RETURNING Lead(Id), Contact(Id)];
    }

    public Result load(String name) {
        Account acct = new Account(Name = name);
        insert acct;
        helper(acct);
        this.audit(acct);
        Formatter.format(name);
        log('loaded');
        return new Result(1);
    }

    private void helper(Account acct) {
        acct.Name = Formatter.format(acct.Name);
    }

    private void audit(Account acct) {
        System.debug(acct);
    }

    public void execute(QueueableContext ctx) {
        load('queued');
    }

    public Database.QueryLocator start(Database.BatchableContext bc) {
        return Database.getQueryLocator('SELECT Id FROM Account');
    }

    public void execute(Database.BatchableContext bc, List<SObject> scope) {}

    public void finish(Database.BatchableContext bc) {}
}
