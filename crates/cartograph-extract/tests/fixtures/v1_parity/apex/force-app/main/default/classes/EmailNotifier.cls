public class EmailNotifier implements AccountService.Notifier, Schedulable {
    public void notify(Account acct) {
        Formatter.format(acct.Name);
    }

    public void execute(SchedulableContext ctx) {
        notify(new Account());
    }
}
