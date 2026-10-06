package com.acme.mapper;

import org.apache.ibatis.annotations.Param;

public interface AuditMapper {
    int record(@Param("action") String action);
}
