public with sharing class OpportunityController {
    @AuraEnabled
    public static List<Opportunity> fetch() {
        return [SELECT Id, Name FROM Opportunity];
    }

    public static List<Opportunity> fetch(Integer max) {
        return [SELECT Id FROM Opportunity LIMIT :max];
    }

    @AuraEnabled
    public static void save(Opportunity record) {
        update record;
    }
}
