package com.acme.mapper;

import com.acme.model.User;
import org.apache.ibatis.annotations.Param;

public interface UserMapper {
    User findById(@Param("id") Long id);

    int insertUser(@Param("user") User user);

    int updateName(@Param("id") Long id, @Param("name") String name);

    int deleteById(@Param("id") Long id);
}
