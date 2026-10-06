@IsTest
private class AccountServiceTest {
    @IsTest
    static void loadsAccounts() {
        AccountService svc = new AccountService();
        AccountService.Result res = svc.load('Acme');
        System.assertEquals(1, res.count);
    }
}
