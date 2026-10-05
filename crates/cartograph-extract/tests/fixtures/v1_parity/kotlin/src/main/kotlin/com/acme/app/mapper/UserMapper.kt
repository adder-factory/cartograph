package com.acme.app.mapper

import com.acme.app.model.User
import org.apache.ibatis.annotations.Mapper
import org.apache.ibatis.annotations.Param

@Mapper
interface UserMapper {
    fun find(@Param("id") id: Long, plain: Int): User

    fun rename(@Param("id") id: Long, @Param("name") name: String): Int
}
